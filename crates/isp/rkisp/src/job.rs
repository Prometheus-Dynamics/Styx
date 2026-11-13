//! The ISP on `camera.isp/1`, through `styx-lemnos`: a job is the parameter buffer for a frame,
//! written into one of the config stream's buffers and posted with the job the contract encodes;
//! the statistics come back on the statistics stream, each carrying its job's identity.
//!
//! A config buffer is held (its lease is out) from its job's post until the statistics of that
//! job or a later one arrive: the ISP has read it by then, since it works through its jobs in
//! order. A frame that finds no free buffer, or a job the queue refuses, is not posted and is
//! counted as dropped by the caller (`Ok(None)`); nothing waits.

use std::collections::VecDeque;
use std::time::Duration;

use lemnos_contract_camera_isp::{
    CameraIsp, ConfigLayout, IspJob, MAX_OUTPUTS, MAX_STREAM_NAME, NO_BUFFER,
};
use lemnos_core::{Encode, Writer};
use styx_aaa::Statistics;
use styx_format::{FourCc, MediaFormat, Resolution};
use styx_frame::{FrameEntry, FrameLease, Layouts, PlaneLayout};
use styx_lemnos::rkisp::Generation;
use styx_lemnos::{Client, DeviceEntry, Frames, JobStream, open_frames, open_job_stream};

use crate::params::Params;
use crate::{Error, statistics};

/// Bytes of the encoded job the scratch buffer holds: the contract's `IspJob` is 36 bytes.
const JOB_SCRATCH: usize = 64;

/// The ISP's job stream and statistics stream, opened from `camera.isp/1`.
pub struct Isp {
    generation: Generation,
    config: JobStream<()>,
    statistics: Frames<()>,
    /// Config buffers held for the jobs the ISP has not yet reported statistics for, oldest first,
    /// each with its job's identity.
    in_flight: VecDeque<(u64, FrameLease<()>)>,
    /// The identity of the next job.
    identity: u64,
}

impl std::fmt::Debug for Isp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Isp")
            .field("generation", &self.generation)
            .field("config_buffers", &self.config.buffers())
            .field("in_flight", &self.in_flight.len())
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

/// What the ISP reports and the stream names it gave, as one stream name.
fn name_of(buf: &[u8], len: usize) -> Result<&str, Error> {
    let bytes = buf
        .get(..len)
        .ok_or_else(|| Error::Device("a stream name longer than its buffer".into()))?;
    std::str::from_utf8(bytes).map_err(|_| Error::Device("a stream name is not UTF-8".into()))
}

/// The reservation's format: the buffer is held for the job; the format means nothing to the ISP.
fn reservation_format() -> Result<MediaFormat, Error> {
    let resolution =
        Resolution::new(1, 1).ok_or_else(|| Error::Buffer("a 1x1 resolution".into()))?;
    Ok(MediaFormat::with_defaults(FourCc::GREY, resolution))
}

impl Isp {
    /// Opens `device` (a `camera.isp/1` device) for jobs and statistics. Its generation is the one
    /// whose statistics layout the device names; a layout this crate does not know is an error.
    pub fn open(client: &Client, device: &DeviceEntry) -> Result<Self, Error> {
        let mut isp = client
            .open::<dyn CameraIsp>(&device.name)
            .map_err(|e| Error::Device(format!("{}: {e}", device.name)))?;
        let info = isp
            .info()
            .map_err(|e| Error::Device(format!("{}: info: {e:?}", device.name)))?;
        let generation = [Generation::Isp21, Generation::Isp30, Generation::Isp35]
            .into_iter()
            .find(|g| g.statistics_layout().layout == info.statistics_layout)
            .ok_or_else(|| {
                Error::Device(format!(
                    "{}: statistics layout {:#x} is not one of the library's",
                    device.name, info.statistics_layout
                ))
            })?;
        let config = generation.config_layout();
        let want = ConfigLayout {
            layout: config.layout,
            bytes: config.bytes,
        };
        let config_layout = isp
            .config_layout(0)
            .map_err(|e| Error::Device(format!("{}: config_layout: {e:?}", device.name)))?;
        if config_layout != want {
            return Err(Error::Device(format!(
                "{}: config layout {config_layout:?} is not the library's {want:?}",
                device.name
            )));
        }
        let mut name = [0u8; MAX_STREAM_NAME];
        let len = isp
            .config_stream(&mut name)
            .map_err(|e| Error::Device(format!("{}: config_stream: {e:?}", device.name)))?;
        let config_name = name_of(&name, len)?.to_owned();
        let len = isp
            .statistics_stream(&mut name)
            .map_err(|e| Error::Device(format!("{}: statistics_stream: {e:?}", device.name)))?;
        let stats_name = name_of(&name, len)?.to_owned();
        drop(isp);

        let config = open_job_stream::<()>(client, device.index, &config_name)
            .map_err(|e| Error::Device(format!("job stream {config_name}: {e}")))?;
        if config.buffer_bytes() < want.bytes as usize {
            return Err(Error::Device(format!(
                "job stream {config_name}: buffers of {} bytes hold no {} byte parameters",
                config.buffer_bytes(),
                want.bytes
            )));
        }
        // Statistics are one row of bytes: the stream's buffers hold the statistics buffer.
        let width = info.statistics_bytes;
        let statistics =
            open_frames::<()>(client, device.index, &stats_name, FourCc::GREY, width, 1)
                .map_err(|e| Error::Device(format!("statistics stream {stats_name}: {e}")))?
                .numbered_by_job();
        Ok(Self {
            generation,
            config,
            statistics,
            in_flight: VecDeque::new(),
            identity: 0,
        })
    }

    /// The ISP's generation, from its statistics layout.
    #[must_use]
    pub fn generation(&self) -> Generation {
        self.generation
    }

    /// Posts `params` as the job of the next frame, with `outputs` (a buffer on each output's
    /// stream, or [`NO_BUFFER`] to skip it). Returns the job's identity, or `None` when no config
    /// buffer is free or the queue is full: the job is dropped, not waited for.
    pub fn post(
        &mut self,
        params: &Params,
        outputs: [u32; MAX_OUTPUTS],
    ) -> Result<Option<u64>, Error> {
        if params.generation() != self.generation {
            return Err(Error::Generation {
                have: self.generation,
                want: params.generation(),
            });
        }
        let pool = self.config.pool();
        let Some(index) = (0..pool.buffers()).find(|&i| pool.is_free(i)) else {
            return Ok(None);
        };
        let identity = self.identity;
        let bytes = params.as_bytes();
        let layouts = Layouts::new(&[PlaneLayout::new(0, bytes.len(), bytes.len())])
            .ok_or_else(|| Error::Buffer("empty parameters".into()))?;
        let entry = FrameEntry::new(identity, 0, reservation_format()?, layouts);
        let mut lease = pool
            .acquire_at(index, entry)
            .map_err(|e| Error::Buffer(format!("config buffer {index}: {e:?}")))?;
        lease
            .write()
            .ok_or_else(|| Error::Buffer("config buffer is not writable".into()))?
            .bytes()
            .get_mut(..bytes.len())
            .ok_or_else(|| Error::Buffer("config buffer is shorter than the parameters".into()))?
            .copy_from_slice(bytes);

        let job = IspJob {
            identity,
            layout: self.generation.config_layout().layout,
            config_buffer: index as u32,
            input_buffer: NO_BUFFER,
            outputs,
        };
        let mut encoded = [0u8; JOB_SCRATCH];
        let mut writer = Writer::new(&mut encoded);
        job.encode(&mut writer)
            .map_err(|_| Error::Buffer("the job does not encode".into()))?;
        let n = writer.len();
        let posted = self
            .config
            .post(&encoded[..n])
            .map_err(|e| Error::Device(format!("posting job {identity}: {e}")))?;
        if !posted {
            return Ok(None);
        }
        self.identity = self.identity.wrapping_add(1);
        self.in_flight.push_back((identity, lease));
        Ok(Some(identity))
    }

    /// Waits up to `timeout` for the next statistics and decodes them into `out` for a frame of
    /// `pixels_per_zone` pixels per AE zone. Returns the identity of the job they came from, or
    /// `None` after the timeout. The config buffers of that job and every earlier one are given
    /// back.
    pub fn statistics(
        &mut self,
        timeout: Duration,
        pixels_per_zone: u32,
        out: &mut Statistics,
    ) -> Result<Option<u64>, Error> {
        let Some(lease) = self
            .statistics
            .next(timeout)
            .map_err(|e| Error::Device(format!("statistics: {e}")))?
        else {
            return Ok(None);
        };
        let id = lease.id();
        while self.in_flight.front().is_some_and(|(job, _)| *job <= id) {
            self.in_flight.pop_front();
        }
        let read = lease
            .read()
            .ok_or_else(|| Error::Buffer("statistics are not readable".into()))?;
        let plane = read
            .plane(0)
            .ok_or_else(|| Error::Buffer("statistics have no plane".into()))?;
        statistics(self.generation, plane.data, pixels_per_zone, out)
            .ok_or_else(|| Error::Buffer("statistics are shorter than the ISP's buffer".into()))?;
        Ok(Some(id))
    }
}
