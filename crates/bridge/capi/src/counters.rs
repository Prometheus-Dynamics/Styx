//! The counters record of the camera service (`styx-counters`' snapshot), read from C.
//!
//! The record's bytes come from the counters ring or a file; [`read`] decodes them with the crate's
//! own reader and gives C the numbers as a flat struct. The text fields (the camera's name, the
//! owner's and the health's reasons, the last error) are not in the struct: a C reader that needs
//! them reads the Rust crate's layout (a documented gap of this surface).

use styx_counters::{
    CameraState, DROP_REASONS, Health, Owner, PublishState, Snapshot, SnapshotError,
};

use crate::status::{E_PROTOCOL, Fail, Res};

/// One camera's counters, as the header's `styx_counter_snapshot`. Codes are plain integers:
/// `owner` 0 none, 1 native, 2 libcamera, 3 v4l2; `state` 0 absent, 1 streaming, 2 retrying,
/// 3 backoff, 4 stopped, 5 off; `health` 0 unknown, 1 healthy, 2 degraded, 3 failed; `publish`
/// 0 waiting, 1 publishing, 2 not publishable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct CounterSnapshot {
    pub seq: u64,
    pub taken_ns: u64,
    pub owner: u32,
    pub state: u32,
    pub health: u32,
    pub publish: u32,
    pub frames: u64,
    pub last_id: u64,
    pub last_timestamp_ns: u64,
    pub published: u64,
    pub ring_full: u64,
    pub readers: u32,
    pub drops: [u64; DROP_REASONS],
    pub copies: u64,
    pub copy_bytes: u64,
    pub age_count: u64,
    pub age_min_ns: u64,
    pub age_median_ns: u64,
    pub age_max_ns: u64,
    pub achieved_fps_milli: u32,
    pub window_span_ns: u64,
    pub window_frames: u32,
    pub requested_fps_milli: u32,
    pub next_delay_ms: u32,
    pub failures: u32,
    pub total_failures: u64,
}

impl From<&Snapshot> for CounterSnapshot {
    fn from(s: &Snapshot) -> Self {
        Self {
            seq: s.seq,
            taken_ns: s.taken_ns,
            owner: match s.owner {
                Owner::None => 0,
                Owner::Native => 1,
                Owner::Libcamera => 2,
                Owner::V4l2 => 3,
            },
            state: match s.state {
                CameraState::Absent => 0,
                CameraState::Streaming => 1,
                CameraState::Retrying => 2,
                CameraState::Backoff => 3,
                CameraState::Stopped => 4,
                CameraState::Off => 5,
            },
            health: match s.health {
                Health::Unknown => 0,
                Health::Healthy => 1,
                Health::Degraded => 2,
                Health::Failed => 3,
            },
            publish: match s.publish {
                PublishState::Waiting => 0,
                PublishState::Publishing => 1,
                PublishState::NotPublishable => 2,
            },
            frames: s.frames,
            last_id: s.last_id,
            last_timestamp_ns: s.last_timestamp_ns,
            published: s.published,
            ring_full: s.ring_full,
            readers: s.readers,
            drops: s.drops,
            copies: s.copies,
            copy_bytes: s.copy_bytes,
            age_count: s.age.count,
            age_min_ns: s.age.min_ns,
            age_median_ns: s.age.median_ns,
            age_max_ns: s.age.max_ns,
            achieved_fps_milli: s.achieved_fps_milli,
            window_span_ns: s.window_span_ns,
            window_frames: s.window_frames,
            requested_fps_milli: s.requested_fps_milli,
            next_delay_ms: s.next_delay_ms,
            failures: s.failures,
            total_failures: s.total_failures,
        }
    }
}

/// Decodes a counters record. Each refusal has its own status: a short record is `E_INVALID`; a
/// record that is not this layout (magic, version, length, or a code no variant has) is
/// `E_PROTOCOL`.
pub(crate) fn read(bytes: &[u8]) -> Res<CounterSnapshot> {
    let snapshot = styx_counters::read_snapshot(bytes).map_err(|e| match e {
        SnapshotError::Short(n) => Fail::invalid(format!(
            "{n} bytes is shorter than a counters record ({} bytes)",
            styx_counters::SNAPSHOT_BYTES
        )),
        SnapshotError::Magic => Fail::new(E_PROTOCOL, "not a counters record (bad magic)"),
        SnapshotError::Version(v) => Fail::new(
            E_PROTOCOL,
            format!("counters record version {v} is not read here"),
        ),
        SnapshotError::Length(n) => Fail::new(
            E_PROTOCOL,
            format!("counters record length {n} is not this layout's"),
        ),
        SnapshotError::Code { field, code } => Fail::new(
            E_PROTOCOL,
            format!("counters record has an unknown {field} code {code}"),
        ),
    })?;
    Ok(CounterSnapshot::from(&snapshot))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::E_INVALID;

    /// A record of zeros with the magic, the version and the length set: a valid, empty snapshot.
    fn empty_record() -> Vec<u8> {
        let mut bytes = vec![0_u8; styx_counters::SNAPSHOT_BYTES];
        bytes[..4].copy_from_slice(&styx_counters::MAGIC);
        bytes[4..6].copy_from_slice(&styx_counters::VERSION.to_le_bytes());
        bytes[6..8].copy_from_slice(&(styx_counters::SNAPSHOT_BYTES as u16).to_le_bytes());
        bytes
    }

    #[test]
    fn a_valid_record_reads_and_each_refusal_has_its_status() {
        let record = read(&empty_record()).expect("a valid record");
        assert_eq!(record, CounterSnapshot::default());
        let short = read(&empty_record()[..100]).map_err(|f| f.status);
        assert_eq!(short, Err(E_INVALID));
        let mut magic = empty_record();
        magic[0] = b'X';
        assert_eq!(read(&magic).map_err(|f| f.status), Err(E_PROTOCOL));
        let mut version = empty_record();
        version[4] = 9;
        assert_eq!(read(&version).map_err(|f| f.status), Err(E_PROTOCOL));
        let mut owner = empty_record();
        owner[89] = 77;
        assert_eq!(read(&owner).map_err(|f| f.status), Err(E_PROTOCOL));
        assert_eq!(read(&[]).map_err(|f| f.status), Err(E_INVALID));
    }
}
