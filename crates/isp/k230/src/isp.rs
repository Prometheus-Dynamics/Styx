//! The live ISP: a frame's register writes posted as an [`IspJob`] on a `camera.isp/1` device's
//! configuration stream, and the statistics the job's frame measured read back from its
//! statistics stream, through `styx-lemnos`.
//!
//! [`Isp::open`] finds the two streams with the device's own `config_stream` and
//! `statistics_stream` calls and opens them. [`Isp::post`] takes a free configuration buffer,
//! lays the [`Params`] out in it, and posts the job; [`Isp::statistics`] waits for the statistics
//! frame that carries the job's identity and decodes it. One job's configuration is copied by the
//! driver before its statistics come out, so the buffer is held until then and no longer.
//!
//! Untested on board: the live path needs a `camera.isp/1` device on Lemnos, which this crate's
//! tests do not run.

use std::time::Duration;

use lemnos_contract_camera_isp::{CameraIsp, IspJob, MAX_OUTPUTS, MAX_STREAM_NAME, NO_BUFFER};
use lemnos_core::{Encode, Writer};
use styx_aaa::Statistics;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{FrameEntry, FrameLease, Layouts, PlaneLayout};
use styx_lemnos::k230isp::stats::Raw;
use styx_lemnos::k230isp::{LAYOUT_WRITES, STATISTICS_BYTES};
use styx_lemnos::{Client, DeviceEntry, Frames, JobStream, open_frames, open_job_stream};

use crate::params::{ConfigError, Params};
use crate::settings;

/// Bytes of an encoded job, with room: an [`IspJob`] is a `u64` and six `u32`s.
const JOB_SCRATCH: usize = 64;

/// Why the live ISP could not run a job.
#[derive(Debug)]
pub enum Error {
    /// The device could not be opened, or did not answer a stream call.
    Device(String),
    /// A Lemnos stream call failed.
    Lemnos(styx_lemnos::Error),
    /// The configuration does not fit Lemnos's layout.
    Config(ConfigError),
    /// A pool, layout or buffer call failed (its text).
    Buffer(String),
    /// Every configuration buffer is still held.
    NoFreeBuffer,
    /// The job queue is full; nothing was posted.
    QueueFull,
    /// No statistics frame for the job came within the timeout.
    Timeout { identity: u64 },
    /// A statistics frame was shorter than the layout (its length in bytes).
    ShortStatistics(usize),
    /// A statistics frame has no bytes to read (its text).
    Unreadable(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Device(s) => write!(f, "camera.isp device: {s}"),
            Self::Lemnos(e) => write!(f, "{e}"),
            Self::Config(e) => write!(f, "{e}"),
            Self::Buffer(s) => write!(f, "buffer: {s}"),
            Self::NoFreeBuffer => write!(f, "no free configuration buffer"),
            Self::QueueFull => write!(f, "the job queue is full"),
            Self::Timeout { identity } => write!(f, "no statistics for job {identity}"),
            Self::ShortStatistics(n) => write!(f, "{n} bytes of statistics, not enough"),
            Self::Unreadable(s) => write!(f, "statistics frame: {s}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<styx_lemnos::Error> for Error {
    fn from(e: styx_lemnos::Error) -> Self {
        Self::Lemnos(e)
    }
}

impl From<ConfigError> for Error {
    fn from(e: ConfigError) -> Self {
        Self::Config(e)
    }
}

/// A `camera.isp/1` device's configuration and statistics streams.
pub struct Isp {
    config: JobStream<()>,
    statistics: Frames<()>,
    /// The configuration buffer of the job in flight, held until its statistics come out.
    in_flight: Option<FrameLease<()>>,
    sequence: u64,
    timeout: Duration,
}

impl std::fmt::Debug for Isp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Isp")
            .field("config", &self.config)
            .field("statistics", &self.statistics)
            .field("sequence", &self.sequence)
            .finish()
    }
}

/// The text a device call answered, as UTF-8.
fn name_of(bytes: &[u8]) -> Result<String, Error> {
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| Error::Device("a stream name is not UTF-8".into()))
}

impl Isp {
    /// Opens `device`'s configuration stream (writable: it is filled and posted on) and its
    /// statistics stream. `timeout` bounds the wait for each job's statistics.
    ///
    /// # Errors
    ///
    /// [`Error::Device`] when the device or a stream name is not there, [`Error::Lemnos`] when a
    /// stream does not open.
    pub fn open(client: &Client, device: &DeviceEntry, timeout: Duration) -> Result<Self, Error> {
        let mut isp = client
            .open::<dyn CameraIsp>(&device.name)
            .map_err(|e| Error::Device(format!("{}: {e}", device.name)))?;
        let mut name = [0u8; MAX_STREAM_NAME];
        let len = isp
            .config_stream(&mut name)
            .map_err(|e| Error::Device(format!("{}: config_stream: {e:?}", device.name)))?;
        let config_name = name_of(&name[..len])?;
        let len = isp
            .statistics_stream(&mut name)
            .map_err(|e| Error::Device(format!("{}: statistics_stream: {e:?}", device.name)))?;
        let stats_name = name_of(&name[..len])?;
        drop(isp);

        let config = open_job_stream::<()>(client, device.index, &config_name)?;
        let statistics = open_frames::<()>(
            client,
            device.index,
            &stats_name,
            FourCc::GREY,
            STATISTICS_BYTES as u32,
            1,
        )?
        .numbered_by_job();
        Ok(Self {
            config,
            statistics,
            in_flight: None,
            sequence: 0,
            timeout,
        })
    }

    /// Posts `params` as one job: its configuration goes into a free buffer of the configuration
    /// stream, and the job names that buffer with [`LAYOUT_WRITES`]. Returns the job's identity,
    /// which the statistics frame carries ([`Isp::statistics`]).
    ///
    /// # Errors
    ///
    /// [`Error::Config`] for a list Lemnos's layout cannot take, [`Error::NoFreeBuffer`] while the
    /// previous job's buffer is still held, [`Error::QueueFull`] when the job ring is full.
    pub fn post(&mut self, params: &Params) -> Result<u64, Error> {
        let len = params.encoded_len()?;
        let identity = self.sequence;
        let pool = self.config.pool();
        let index = (0..pool.buffers())
            .find(|&i| pool.is_free(i))
            .ok_or(Error::NoFreeBuffer)?;
        let layouts = Layouts::new(&[PlaneLayout::new(0, len, len)])
            .ok_or_else(|| Error::Buffer("the configuration layout".into()))?;
        let format = MediaFormat::with_defaults(
            FourCc::GREY,
            Resolution::new(1, 1).ok_or_else(|| Error::Buffer("a 1x1 resolution".into()))?,
        );
        let mut lease = pool
            .acquire_at(index, FrameEntry::new(identity, 0, format, layouts))
            .map_err(|e| Error::Buffer(format!("configuration buffer {index}: {e:?}")))?;
        let mut planes = lease
            .write()
            .ok_or_else(|| Error::Buffer("the configuration buffer is not writable".into()))?;
        let bytes = planes
            .bytes()
            .get_mut(..len)
            .ok_or_else(|| Error::Buffer("the configuration is larger than its buffer".into()))?;
        params.encode(bytes)?;

        let job = IspJob {
            identity,
            layout: LAYOUT_WRITES,
            config_buffer: index as u32,
            input_buffer: NO_BUFFER,
            outputs: [NO_BUFFER; MAX_OUTPUTS],
        };
        let mut scratch = [0u8; JOB_SCRATCH];
        let mut writer = Writer::new(&mut scratch);
        job.encode(&mut writer)
            .map_err(|_| Error::Buffer("the job does not encode".into()))?;
        let n = writer.len();
        if !self.config.post(&scratch[..n])? {
            return Err(Error::QueueFull);
        }
        self.sequence = self.sequence.wrapping_add(1);
        self.in_flight = Some(lease);
        Ok(identity)
    }

    /// Waits up to the timeout for the statistics of job `identity`, decodes them into `out` for
    /// a `width` x `height` frame (see [`settings::statistics`]), and gives the buffer back.
    /// Statistics of other jobs that arrive first are passed over.
    ///
    /// # Errors
    ///
    /// [`Error::Timeout`] when they do not come, [`Error::ShortStatistics`] or
    /// [`Error::Unreadable`] when the frame is not a statistics buffer.
    pub fn statistics(
        &mut self,
        identity: u64,
        (width, height): (u32, u32),
        out: &mut Statistics,
    ) -> Result<(), Error> {
        loop {
            let Some(lease) = self.statistics.next(self.timeout)? else {
                return Err(Error::Timeout { identity });
            };
            if lease.id() != identity {
                continue;
            }
            let read = lease
                .read()
                .ok_or_else(|| Error::Unreadable("no bytes".into()))?;
            let plane = read
                .plane(0)
                .ok_or_else(|| Error::Unreadable("no plane".into()))?;
            let raw = Raw::decode(plane.data).ok_or(Error::ShortStatistics(plane.data.len()))?;
            settings::statistics(&raw, width, height, out);
            self.in_flight = None;
            return Ok(());
        }
    }
}
