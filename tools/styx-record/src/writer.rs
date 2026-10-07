//! The disk writer: its own thread behind a bounded queue, so a slow disk never holds frames
//! the camera service wants back. The recorder copies each frame's Y plane into a buffer from
//! the writer's free list and queues it; when the queue is full the frame is dropped and
//! counted (the disk is not keeping up).

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError, channel, sync_channel};
use std::thread::JoinHandle;

use crate::sha256::Sha256;
use crate::sidecar::{CSV_HEADER, Row};

/// What the writer is asked to write.
pub enum Job {
    /// A video frame, appended to the raw file.
    Frame { luma: Vec<u8>, row: Row },
    /// A still: its own raw file, PGM and Eidos capture metadata.
    Still {
        luma: Vec<u8>,
        row: Row,
        stem: String,
        meta: String,
    },
}

/// Where the files go.
#[derive(Clone, Debug)]
pub struct Layout {
    pub dir: PathBuf,
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// Overwrite existing files.
    pub force: bool,
}

impl Layout {
    /// `<name>_<W>x<H>_gray.raw`: the Eidos raw grey video name (`recording_7d7ca24f_1280x800_gray.raw`).
    pub fn raw_name(&self) -> String {
        format!("{}_{}x{}_gray.raw", self.name, self.width, self.height)
    }

    pub fn csv_name(&self) -> String {
        format!("{}.frames.csv", self.name)
    }

    /// A still's stem: `<name>_still<NNN>_<W>x<H>` (`.gray.raw`, `.pgm`, `.txt`).
    pub fn still_stem(&self, index: u64) -> String {
        format!(
            "{}_still{:03}_{}x{}",
            self.name, index, self.width, self.height
        )
    }

    fn create(&self, path: &Path) -> io::Result<File> {
        let mut o = OpenOptions::new();
        o.write(true);
        if self.force {
            o.create(true).truncate(true);
        } else {
            o.create_new(true);
        }
        o.open(path).map_err(|err| {
            if err.kind() == io::ErrorKind::AlreadyExists {
                io::Error::new(
                    err.kind(),
                    format!("{} exists (use --force)", path.display()),
                )
            } else {
                io::Error::new(err.kind(), format!("{}: {err}", path.display()))
            }
        })
    }
}

/// What the writer wrote.
#[derive(Clone, Debug, Default)]
pub struct Written {
    pub frames: u64,
    pub raw_bytes: u64,
    pub raw_sha256: Option<String>,
    pub stills: Vec<String>,
    pub error: Option<String>,
}

pub struct Writer {
    jobs: Option<SyncSender<Job>>,
    free: Receiver<Vec<u8>>,
    allocated: usize,
    max_buffers: usize,
    queued: Arc<AtomicUsize>,
    failed: Arc<AtomicBool>,
    pub max_queued: usize,
    /// Frames dropped because the queue was full.
    pub dropped: u64,
    thread: Option<JoinHandle<Written>>,
}

struct Thread {
    layout: Layout,
    raw: Option<BufWriter<File>>,
    raw_path: Option<PathBuf>,
    csv: BufWriter<File>,
    sha: Sha256,
    line: String,
    written: Written,
    free: Sender<Vec<u8>>,
    queued: Arc<AtomicUsize>,
    failed: Arc<AtomicBool>,
}

impl Writer {
    /// Create the files (the raw video file unless `stills`) and start the writer with room for
    /// `queue` frames.
    pub fn start(layout: Layout, stills: bool, queue: usize) -> io::Result<Self> {
        std::fs::create_dir_all(&layout.dir)?;
        let (raw, raw_path) = if stills {
            (None, None)
        } else {
            let path = layout.dir.join(layout.raw_name());
            (
                Some(BufWriter::with_capacity(4 << 20, layout.create(&path)?)),
                Some(path),
            )
        };
        let mut csv = BufWriter::new(layout.create(&layout.dir.join(layout.csv_name()))?);
        writeln!(csv, "{CSV_HEADER}")?;
        csv.flush()?;
        let (jobs, rx) = sync_channel(queue);
        let (free_tx, free) = channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicBool::new(false));
        let mut thread = Thread {
            layout,
            raw,
            raw_path,
            csv,
            sha: Sha256::default(),
            line: String::new(),
            written: Written::default(),
            free: free_tx,
            queued: queued.clone(),
            failed: failed.clone(),
        };
        let handle = std::thread::Builder::new()
            .name("styx-record-disk".into())
            .spawn(move || {
                thread.run(rx);
                thread.finish()
            })?;
        Ok(Self {
            jobs: Some(jobs),
            free,
            allocated: 0,
            // The queue, one being written, one being filled.
            max_buffers: queue + 2,
            queued,
            failed,
            max_queued: 0,
            dropped: 0,
            thread: Some(handle),
        })
    }

    /// A buffer to copy a frame into: a free one, else a new one while under the limit; `None`
    /// when every buffer is queued or being written (the frame must be dropped).
    pub fn buffer(&mut self) -> Option<Vec<u8>> {
        if let Ok(buf) = self.free.try_recv() {
            return Some(buf);
        }
        (self.allocated < self.max_buffers).then(|| {
            self.allocated += 1;
            Vec::new()
        })
    }

    /// Queue `job`; when the queue is full it is dropped (counted) and its buffer kept.
    pub fn submit(&mut self, job: Job) -> bool {
        let Some(jobs) = &self.jobs else {
            return false;
        };
        let queued = self.queued.fetch_add(1, Ordering::Relaxed) + 1;
        match jobs.try_send(job) {
            Ok(()) => {
                self.max_queued = self.max_queued.max(queued);
                true
            }
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.queued.fetch_sub(1, Ordering::Relaxed);
                self.dropped += 1;
                false
            }
        }
    }

    /// Frames waiting to be written.
    pub fn queued(&self) -> usize {
        self.queued.load(Ordering::Relaxed)
    }

    /// Writing failed (disk full, I/O error): recording should stop.
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// Write what is queued, close and sync the files.
    pub fn finish(mut self) -> Written {
        self.jobs = None;
        self.thread
            .take()
            .and_then(|t| t.join().ok())
            .unwrap_or_else(|| Written {
                error: Some("the disk writer panicked".into()),
                ..Written::default()
            })
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.jobs = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// `fsync`, except on what cannot be synced (a pipe or a character device as the output).
fn sync(file: &File) -> io::Result<()> {
    match file.sync_all() {
        Err(err) if err.kind() == io::ErrorKind::InvalidInput => Ok(()),
        other => other,
    }
}

impl Thread {
    fn run(&mut self, jobs: Receiver<Job>) {
        for job in jobs {
            let result = match job {
                Job::Frame { luma, row } => {
                    let r = self.frame(&luma, &row);
                    let _ = self.free.send(luma);
                    r
                }
                Job::Still {
                    luma,
                    row,
                    stem,
                    meta,
                } => {
                    let r = self.still(&luma, row, &stem, &meta);
                    let _ = self.free.send(luma);
                    r
                }
            };
            self.queued.fetch_sub(1, Ordering::Relaxed);
            if let Err(err) = result
                && self.written.error.is_none()
            {
                self.written.error = Some(err.to_string());
                self.failed.store(true, Ordering::Relaxed);
            }
        }
    }

    fn frame(&mut self, luma: &[u8], row: &Row) -> io::Result<()> {
        if self.written.error.is_some() {
            return Ok(());
        }
        let raw = self.raw.as_mut().ok_or(io::ErrorKind::Unsupported)?;
        raw.write_all(luma)?;
        self.sha.update(luma);
        self.written.frames += 1;
        self.written.raw_bytes += luma.len() as u64;
        self.row(row)?;
        // Keep the sidecar close to the raw file on disk (a crash loses little of either).
        if self.written.frames.is_multiple_of(32) {
            self.csv.flush()?;
        }
        Ok(())
    }

    fn row(&mut self, row: &Row) -> io::Result<()> {
        self.line.clear();
        row.write_csv(&mut self.line);
        self.csv.write_all(self.line.as_bytes())
    }

    fn still(&mut self, luma: &[u8], mut row: Row, stem: &str, meta: &str) -> io::Result<()> {
        if self.written.error.is_some() {
            return Ok(());
        }
        let dir = self.layout.dir.clone();
        let raw = format!("{stem}.gray.raw");
        let mut f = self.layout.create(&dir.join(&raw))?;
        f.write_all(luma)?;
        f.sync_all()?;
        let mut pgm = self.layout.create(&dir.join(format!("{stem}.pgm")))?;
        write!(
            pgm,
            "P5\n{} {}\n255\n",
            self.layout.width, self.layout.height
        )?;
        pgm.write_all(luma)?;
        pgm.sync_all()?;
        let mut txt = self.layout.create(&dir.join(format!("{stem}.txt")))?;
        txt.write_all(meta.as_bytes())?;
        txt.sync_all()?;
        row.file = Some(raw.clone());
        self.row(&row)?;
        self.csv.flush()?;
        self.written.stills.push(raw);
        Ok(())
    }

    fn finish(mut self) -> Written {
        let mut close = || -> io::Result<()> {
            self.csv.flush()?;
            sync(self.csv.get_ref())?;
            if let Some(mut raw) = self.raw.take() {
                let flushed = raw.flush();
                let (file, _) = raw.into_parts();
                if flushed.is_err() || self.written.error.is_some() {
                    // Whole frames only: a frame cut short by a failed write would break
                    // readers (Eidos refuses a partial frame at the end).
                    let frame = u64::from(self.layout.width) * u64::from(self.layout.height);
                    let len = file.metadata()?.len().min(self.written.raw_bytes);
                    let len = len - len % frame.max(1);
                    file.set_len(len)?;
                    self.written.raw_bytes = len;
                    self.written.frames = len / frame.max(1);
                }
                sync(&file)?;
                flushed?;
            }
            Ok(())
        };
        if let Err(err) = close()
            && self.written.error.is_none()
        {
            self.written.error = Some(err.to_string());
        }
        if self.raw_path.is_some() && self.written.error.is_none() {
            self.written.raw_sha256 = Some(std::mem::take(&mut self.sha).hex());
        }
        self.written
    }
}
