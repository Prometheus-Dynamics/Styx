//! Debug probe for slice timing (`STYX_PISP_FE_LINES_PROBE=1`, needs Styx's rp1-cfe module):
//! a thread subscribed to the front end's line event (`STYX_CFE_EVENT_LINES`) copies a few
//! rows around the event's line from every raw buffer the moment it arrives; when the frame's
//! buffer is dequeued, the copies are compared with the final rows. A row equal to its final
//! content was already in memory at the event (sensor noise makes consecutive frames differ),
//! so the counts give how far behind the event line the data is safe to read.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use styx_kernel::event::{EventKind, EventType, Events, SubscribeFlags};
use styx_kernel::v4l2::VideoDevice;

/// `STYX_CFE_EVENT_LINES - V4L2_EVENT_PRIVATE_START` (kernel-modules/rp1-cfe).
pub const LINES_EVENT_OFFSET: u32 = 0x5354;

/// Rows sampled, relative to the event's line.
const OFFSETS: [i32; 10] = [-128, -64, -32, -16, -8, -4, -1, 0, 4, 16];

type Samples = Arc<Mutex<HashMap<u32, (u32, Vec<Vec<Vec<u8>>>)>>>;

pub(super) struct LinesProbe {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    samples: Samples,
    /// Per offset: (frames compared, rows equal to their final content).
    tally: [(u32, u32); OFFSETS.len()],
    frames: u32,
}

/// A raw buffer's mapping, read by the probe thread (the device outlives it: the thread is
/// joined in `Drop`).
#[derive(Clone, Copy)]
struct Buf {
    ptr: usize,
    len: usize,
}

impl LinesProbe {
    /// Started when `STYX_PISP_FE_LINES_PROBE=1`; `bufs` are the raw buffers' (pointer, length),
    /// `stride` their row stride.
    pub(super) fn start(
        node: &std::path::Path,
        bufs: Vec<(*const u8, usize)>,
        stride: usize,
    ) -> Option<Self> {
        if std::env::var("STYX_PISP_FE_LINES_PROBE").as_deref() != Ok("1") {
            return None;
        }
        let dev = VideoDevice::open(node)
            .map_err(|e| eprintln!("lines probe: open {}: {e}", node.display()))
            .ok()?;
        dev.subscribe(
            EventType::Private(LINES_EVENT_OFFSET),
            0,
            SubscribeFlags::empty(),
        )
        .map_err(|e| eprintln!("lines probe: subscribe (Styx rp1-cfe module loaded?): {e}"))
        .ok()?;
        let bufs: Vec<Buf> = bufs
            .into_iter()
            .map(|(p, len)| Buf {
                ptr: p as usize,
                len,
            })
            .collect();
        let stop = Arc::new(AtomicBool::new(false));
        let samples: Samples = Arc::default();
        let (s2, smp) = (stop.clone(), samples.clone());
        let thread = std::thread::Builder::new()
            .name("styx-lines-probe".into())
            .spawn(move || {
                while !s2.load(Ordering::Relaxed) {
                    let Ok(r) = dev.wait(Some(Duration::from_millis(100))) else {
                        break;
                    };
                    if !r.priority {
                        continue;
                    }
                    while let Ok(Some(ev)) = dev.dequeue_event() {
                        let EventKind::Private { data, .. } = ev.kind else {
                            continue;
                        };
                        let seq = u32::from_ne_bytes(data[0..4].try_into().unwrap());
                        let line = u32::from_ne_bytes(data[4..8].try_into().unwrap());
                        let copies = bufs
                            .iter()
                            .map(|b| {
                                // SAFETY: the mapping lives until the probe is dropped.
                                let all = unsafe {
                                    std::slice::from_raw_parts(b.ptr as *const u8, b.len)
                                };
                                OFFSETS
                                    .iter()
                                    .map(|o| row(all, stride, line as i32 + o).to_vec())
                                    .collect()
                            })
                            .collect();
                        let mut m = smp.lock().unwrap();
                        m.retain(|k, _| seq.wrapping_sub(*k) < 32);
                        m.insert(seq, (line, copies));
                    }
                }
            })
            .ok()?;
        eprintln!("lines probe: on {}", node.display());
        Some(Self {
            stop,
            thread: Some(thread),
            samples,
            tally: [(0, 0); OFFSETS.len()],
            frames: 0,
        })
    }

    /// Frame `seq` landed in buffer `index` whose final content is `data`.
    pub(super) fn check(&mut self, seq: u32, index: u32, data: &[u8], stride: usize) {
        let Some((line, copies)) = self.samples.lock().unwrap().remove(&seq) else {
            return;
        };
        let Some(rows) = copies.get(index as usize) else {
            return;
        };
        for (k, (o, early)) in OFFSETS.iter().zip(rows).enumerate() {
            let fin = row(data, stride, line as i32 + o);
            if fin.is_empty() {
                continue;
            }
            self.tally[k].0 += 1;
            self.tally[k].1 += u32::from(fin == early.as_slice());
        }
        self.frames += 1;
        if self.frames.is_multiple_of(100) {
            let s: Vec<String> = OFFSETS
                .iter()
                .zip(self.tally)
                .map(|(o, (n, eq))| {
                    format!("{o:+}:{:.0}%", 100.0 * f64::from(eq) / f64::from(n.max(1)))
                })
                .collect();
            eprintln!(
                "lines probe: event at line {line}, {} frames; rows already final at the event: {}",
                self.frames,
                s.join(" ")
            );
        }
    }
}

fn row(all: &[u8], stride: usize, y: i32) -> &[u8] {
    let Ok(y) = usize::try_from(y) else {
        return &[];
    };
    all.get(y * stride..(y + 1) * stride).unwrap_or(&[])
}

impl Drop for LinesProbe {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
