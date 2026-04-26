use std::sync::{Arc, Mutex};

use styx_core::controls::{ControlId, ControlValue};

use crate::capture_api::CaptureError;

const CTRL_FILE_VIDEO_PLAYBACK_SPEED_BASE: u32 = 0xF200_0000;
const CTRL_FILE_VIDEO_START_FRAME_BASE: u32 = 0xF210_0000;
const CTRL_FILE_VIDEO_STOP_FRAME_BASE: u32 = 0xF220_0000;
const CTRL_FILE_IMAGE_DURATION_FRAMES_BASE: u32 = 0xF230_0000;
const CTRL_FILE_CONTROL_INDEX_LIMIT: u32 = 0x0001_0000;

fn make_indexed_control_id(base: u32, index: usize) -> ControlId {
    let idx = u32::try_from(index)
        .unwrap_or(u32::MAX)
        .min(CTRL_FILE_CONTROL_INDEX_LIMIT.saturating_sub(1));
    ControlId(base.saturating_add(idx))
}

fn decode_indexed_control_id(id: ControlId, base: u32) -> Option<usize> {
    let end = base.saturating_add(CTRL_FILE_CONTROL_INDEX_LIMIT);
    if id.0 >= base && id.0 < end {
        return usize::try_from(id.0 - base).ok();
    }
    None
}

#[cfg(feature = "file-backend-video")]
pub(crate) fn control_id_file_video_playback_speed(index: usize) -> ControlId {
    make_indexed_control_id(CTRL_FILE_VIDEO_PLAYBACK_SPEED_BASE, index)
}

#[cfg(feature = "file-backend-video")]
pub(crate) fn control_id_file_video_start_frame(index: usize) -> ControlId {
    make_indexed_control_id(CTRL_FILE_VIDEO_START_FRAME_BASE, index)
}

#[cfg(feature = "file-backend-video")]
pub(crate) fn control_id_file_video_stop_frame(index: usize) -> ControlId {
    make_indexed_control_id(CTRL_FILE_VIDEO_STOP_FRAME_BASE, index)
}

pub(crate) fn control_id_file_image_duration_frames(index: usize) -> ControlId {
    make_indexed_control_id(CTRL_FILE_IMAGE_DURATION_FRAMES_BASE, index)
}

#[cfg(feature = "file-backend-video")]
pub(crate) fn control_name_file_video_playback_speed(name: &str) -> String {
    format!("file.video.{name}.playback_speed")
}

#[cfg(feature = "file-backend-video")]
pub(crate) fn control_name_file_video_start_frame(name: &str) -> String {
    format!("file.video.{name}.start_frame")
}

#[cfg(feature = "file-backend-video")]
pub(crate) fn control_name_file_video_stop_frame(name: &str) -> String {
    format!("file.video.{name}.stop_frame")
}

pub(crate) fn control_name_file_image_duration_frames(name: &str) -> String {
    format!("file.image.{name}.duration_frames")
}

#[derive(Debug, Clone)]
pub struct FileControlState {
    pub image_duration_frames: Vec<u32>,
    pub video_playback_speed: Vec<f32>,
    pub video_start_frame: Vec<u32>,
    pub video_stop_frame: Vec<u32>,
    pub video_frame_max: Vec<Option<u32>>,
}

pub(crate) type FileControlStateHandle = Arc<Mutex<FileControlState>>;

pub(crate) fn apply_file_control(
    state: &FileControlStateHandle,
    id: ControlId,
    value: ControlValue,
) -> Result<(), CaptureError> {
    let mut guard = state
        .lock()
        .map_err(|_| CaptureError::control_apply("file control lock poisoned"))?;
    apply_control_to_state(&mut guard, id, value)
}

pub(crate) fn read_file_control(
    state: &FileControlStateHandle,
    id: ControlId,
) -> Result<ControlValue, CaptureError> {
    let guard = state
        .lock()
        .map_err(|_| CaptureError::control_apply("file control lock poisoned"))?;

    if let Some(index) = decode_indexed_control_id(id, CTRL_FILE_VIDEO_PLAYBACK_SPEED_BASE) {
        return guard
            .video_playback_speed
            .get(index)
            .copied()
            .map(ControlValue::Float)
            .ok_or(CaptureError::ControlUnsupported);
    }
    if let Some(index) = decode_indexed_control_id(id, CTRL_FILE_VIDEO_START_FRAME_BASE) {
        return guard
            .video_start_frame
            .get(index)
            .copied()
            .map(ControlValue::Uint)
            .ok_or(CaptureError::ControlUnsupported);
    }
    if let Some(index) = decode_indexed_control_id(id, CTRL_FILE_VIDEO_STOP_FRAME_BASE) {
        return guard
            .video_stop_frame
            .get(index)
            .copied()
            .map(ControlValue::Uint)
            .ok_or(CaptureError::ControlUnsupported);
    }
    if let Some(index) = decode_indexed_control_id(id, CTRL_FILE_IMAGE_DURATION_FRAMES_BASE) {
        return guard
            .image_duration_frames
            .get(index)
            .copied()
            .map(ControlValue::Uint)
            .ok_or(CaptureError::ControlUnsupported);
    }

    Err(CaptureError::ControlUnsupported)
}

pub(crate) fn parse_controls(
    controls: &[(ControlId, ControlValue)],
    image_count: usize,
    video_frame_max: Vec<Option<u32>>,
) -> FileControlState {
    let video_count = video_frame_max.len();
    let mut state = FileControlState {
        image_duration_frames: vec![1; image_count],
        video_playback_speed: vec![1.0; video_count],
        video_start_frame: vec![0; video_count],
        video_stop_frame: vec![0; video_count],
        video_frame_max,
    };

    for (id, val) in controls {
        let _ = apply_control_to_state(&mut state, *id, val.clone());
    }

    state
}

fn apply_control_to_state(
    state: &mut FileControlState,
    id: ControlId,
    value: ControlValue,
) -> Result<(), CaptureError> {
    if let Some(index) = decode_indexed_control_id(id, CTRL_FILE_VIDEO_PLAYBACK_SPEED_BASE) {
        let slot = state
            .video_playback_speed
            .get_mut(index)
            .ok_or(CaptureError::ControlUnsupported)?;
        *slot = match value {
            ControlValue::Float(v) if v > 0.0 => v,
            ControlValue::Uint(v) if v > 0 => v as f32,
            ControlValue::Int(v) if v > 0 => v as f32,
            _ => return Err(CaptureError::ControlUnsupported),
        };
        return Ok(());
    }

    if let Some(index) = decode_indexed_control_id(id, CTRL_FILE_VIDEO_START_FRAME_BASE) {
        let frame_max = state.video_frame_max.get(index).copied().flatten();
        let slot = state
            .video_start_frame
            .get_mut(index)
            .ok_or(CaptureError::ControlUnsupported)?;
        let mut next = match value {
            ControlValue::Uint(v) => v,
            ControlValue::Int(v) if v >= 0 => v as u32,
            _ => return Err(CaptureError::ControlUnsupported),
        };
        if let Some(max) = frame_max {
            next = next.min(max);
        }
        *slot = next;
        if let Some(stop_slot) = state.video_stop_frame.get_mut(index) {
            if let Some(max) = frame_max {
                *stop_slot = (*stop_slot).min(max);
            }
            if *slot > *stop_slot {
                *stop_slot = *slot;
            }
        }
        return Ok(());
    }

    if let Some(index) = decode_indexed_control_id(id, CTRL_FILE_VIDEO_STOP_FRAME_BASE) {
        let frame_max = state.video_frame_max.get(index).copied().flatten();
        let slot = state
            .video_stop_frame
            .get_mut(index)
            .ok_or(CaptureError::ControlUnsupported)?;
        let mut next = match value {
            ControlValue::Uint(v) => v,
            ControlValue::Int(v) if v >= 0 => v as u32,
            _ => return Err(CaptureError::ControlUnsupported),
        };
        if let Some(max) = frame_max {
            next = next.min(max);
        }
        *slot = next;
        if let Some(start_slot) = state.video_start_frame.get_mut(index) {
            if let Some(max) = frame_max {
                *start_slot = (*start_slot).min(max);
            }
            if *slot < *start_slot {
                *start_slot = *slot;
            }
        }
        return Ok(());
    }

    if let Some(index) = decode_indexed_control_id(id, CTRL_FILE_IMAGE_DURATION_FRAMES_BASE) {
        let slot = state
            .image_duration_frames
            .get_mut(index)
            .ok_or(CaptureError::ControlUnsupported)?;
        *slot = match value {
            ControlValue::Uint(v) if v > 0 => v,
            ControlValue::Int(v) if v > 0 => v as u32,
            _ => return Err(CaptureError::ControlUnsupported),
        };
        return Ok(());
    }

    Err(CaptureError::ControlUnsupported)
}

#[cfg(all(test, feature = "file-backend-video"))]
mod tests {
    use super::*;

    fn state_with_one_video() -> FileControlState {
        FileControlState {
            image_duration_frames: vec![],
            video_playback_speed: vec![1.0],
            video_start_frame: vec![0],
            video_stop_frame: vec![0],
            video_frame_max: vec![None],
        }
    }

    #[test]
    fn start_frame_updates_stop_when_crossing() {
        let mut state = state_with_one_video();
        apply_control_to_state(
            &mut state,
            control_id_file_video_stop_frame(0),
            ControlValue::Uint(100),
        )
        .expect("set stop");
        apply_control_to_state(
            &mut state,
            control_id_file_video_start_frame(0),
            ControlValue::Uint(150),
        )
        .expect("set start");

        assert_eq!(state.video_start_frame[0], 150);
        assert_eq!(state.video_stop_frame[0], 150);
    }

    #[test]
    fn stop_frame_updates_start_when_crossing() {
        let mut state = state_with_one_video();
        apply_control_to_state(
            &mut state,
            control_id_file_video_start_frame(0),
            ControlValue::Uint(200),
        )
        .expect("set start");
        apply_control_to_state(
            &mut state,
            control_id_file_video_stop_frame(0),
            ControlValue::Uint(120),
        )
        .expect("set stop");

        assert_eq!(state.video_start_frame[0], 120);
        assert_eq!(state.video_stop_frame[0], 120);
    }

    #[test]
    fn frame_window_clamps_to_known_max() {
        let mut state = FileControlState {
            image_duration_frames: vec![],
            video_playback_speed: vec![1.0],
            video_start_frame: vec![0],
            video_stop_frame: vec![0],
            video_frame_max: vec![Some(42)],
        };

        apply_control_to_state(
            &mut state,
            control_id_file_video_start_frame(0),
            ControlValue::Uint(1000),
        )
        .expect("set start");
        apply_control_to_state(
            &mut state,
            control_id_file_video_stop_frame(0),
            ControlValue::Uint(2000),
        )
        .expect("set stop");

        assert_eq!(state.video_start_frame[0], 42);
        assert_eq!(state.video_stop_frame[0], 42);
    }
}
