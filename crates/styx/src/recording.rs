use std::borrow::Cow;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use image::{ColorType, ImageFormat, save_buffer_with_format};
use styx_core::prelude::{FourCc, FrameLease};

/// Recording output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingFormat {
    /// Pick a sensible format based on the frame format.
    ///
    /// MJPG/JPEG frames are written as `.jpg`; everything else is stored as `.png`.
    Auto,
    /// Encode frames as PNG images.
    Png,
    /// Encode frames as JPEG images (or pass through MJPG/JPEG frames).
    Jpeg,
}

impl RecordingFormat {
    fn extension_for(self, code: FourCc) -> &'static str {
        match self {
            RecordingFormat::Auto => {
                if is_jpeg_fourcc(code) {
                    "jpg"
                } else {
                    "png"
                }
            }
            RecordingFormat::Png => "png",
            RecordingFormat::Jpeg => "jpg",
        }
    }
}

/// Configuration for a `FrameRecorder`.
#[derive(Debug, Clone)]
pub struct RecordingOptions {
    /// File name prefix (e.g. `frame_000001.png`).
    pub prefix: String,
    /// Output format.
    pub format: RecordingFormat,
    /// Zero-padding width for sequence numbers.
    pub zero_pad: usize,
    /// Starting index for the sequence counter.
    pub start_index: u64,
    /// Stable recording session id written to metadata and index files.
    pub session_id: Option<String>,
}

impl Default for RecordingOptions {
    fn default() -> Self {
        Self {
            prefix: "frame".into(),
            format: RecordingFormat::Auto,
            zero_pad: 6,
            start_index: 0,
            session_id: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingSessionMetadata {
    pub session_id: String,
    pub started_unix_ms: u128,
    pub directory: PathBuf,
    pub prefix: String,
    pub format: RecordingFormat,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingFrameIndexEntry {
    pub sequence: u64,
    pub timestamp: u64,
    pub fourcc: FourCc,
    pub width: u32,
    pub height: u32,
    pub payload_bytes: usize,
    pub path: PathBuf,
}

/// Errors emitted by the recording helpers.
#[derive(Debug, thiserror::Error)]
pub enum RecordingError {
    #[error("recording io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("recording image error: {0}")]
    Image(#[from] image::ImageError),
    #[error("frame contains no data")]
    EmptyFrame,
    #[error("frame format {0} is not supported for recording")]
    UnsupportedFormat(String),
}

/// Record a stream of frames to disk as numbered image files.
///
/// # Example
/// ```rust,no_run
/// use styx::prelude::*;
///
/// let device = make_virtual_rgb_device("virtual", 640, 360, 30);
/// let recorder = FrameRecorder::new("./recordings", RecordingOptions::default())?;
/// let mut pipeline = MediaPipelineBuilder::new(CaptureRequest::new(&device))
///     .sink("recording", recorder)
///     .start()?;
///
/// loop {
///     match pipeline.try_next_result()? {
///         RecvOutcome::Data(_) => {}
///         RecvOutcome::Empty | RecvOutcome::Closed => break,
///     }
/// }
/// let recorder = pipeline.stop_with_recorder().expect("recorder");
/// let _paths = recorder.into_paths();
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct FrameRecorder {
    dir: PathBuf,
    options: RecordingOptions,
    index: u64,
    paths: Vec<PathBuf>,
    metadata: RecordingSessionMetadata,
    index_path: PathBuf,
    error_count: usize,
    last_error: Option<String>,
}

impl FrameRecorder {
    /// Create a recorder that writes numbered image files to `dir`.
    pub fn new(dir: impl Into<PathBuf>, options: RecordingOptions) -> Result<Self, RecordingError> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        let started_unix_ms = now_unix_ms();
        let session_id = options
            .session_id
            .clone()
            .unwrap_or_else(|| format!("styx-{started_unix_ms}-{}", std::process::id()));
        let metadata = RecordingSessionMetadata {
            session_id,
            started_unix_ms,
            directory: dir.clone(),
            prefix: options.prefix.clone(),
            format: options.format,
        };
        let metadata_path = dir.join("session.json");
        let index_path = dir.join("index.tsv");
        write_session_metadata(&metadata_path, &metadata)?;
        write_index_header(&index_path)?;
        Ok(Self {
            dir,
            index: options.start_index,
            options,
            paths: Vec::new(),
            metadata,
            index_path,
            error_count: 0,
            last_error: None,
        })
    }

    /// Record a single frame to disk.
    pub fn record(&mut self, frame: &FrameLease) -> Result<PathBuf, RecordingError> {
        let result = self.record_inner(frame);
        if let Err(ref err) = result {
            self.error_count = self.error_count.saturating_add(1);
            self.last_error = Some(err.to_string());
        }
        result
    }

    /// Recorded file paths in capture order.
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    pub fn metadata(&self) -> &RecordingSessionMetadata {
        &self.metadata
    }

    pub fn index_path(&self) -> &Path {
        &self.index_path
    }

    pub fn next_sequence(&self) -> u64 {
        self.index
    }

    /// Consume the recorder and return recorded paths.
    pub fn into_paths(self) -> Vec<PathBuf> {
        self.paths
    }

    /// Number of recording failures encountered so far.
    pub fn error_count(&self) -> usize {
        self.error_count
    }

    /// Last recording error, if any.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    fn record_inner(&mut self, frame: &FrameLease) -> Result<PathBuf, RecordingError> {
        let code = frame.meta().format.code;
        let ext = self.options.format.extension_for(code);
        let path = self.next_path(ext);

        match self.options.format {
            RecordingFormat::Auto => {
                if is_jpeg_fourcc(code) {
                    write_encoded_frame(frame, &path)?;
                } else {
                    save_frame_image(frame, &path, ImageFormat::Png)?;
                }
            }
            RecordingFormat::Png => {
                save_frame_image(frame, &path, ImageFormat::Png)?;
            }
            RecordingFormat::Jpeg => {
                if is_jpeg_fourcc(code) {
                    write_encoded_frame(frame, &path)?;
                } else {
                    save_frame_image(frame, &path, ImageFormat::Jpeg)?;
                }
            }
        }

        self.paths.push(path.clone());
        append_index_entry(&self.index_path, &self.index_entry(frame, &path))?;
        self.index = self.index.saturating_add(1);
        Ok(path)
    }

    fn index_entry(&self, frame: &FrameLease, path: &Path) -> RecordingFrameIndexEntry {
        let res = frame.meta().format.resolution;
        RecordingFrameIndexEntry {
            sequence: self.index,
            timestamp: frame.meta().timestamp,
            fourcc: frame.meta().format.code,
            width: res.width.get(),
            height: res.height.get(),
            payload_bytes: frame.payload_bytes(),
            path: path.to_path_buf(),
        }
    }

    fn next_path(&self, ext: &str) -> PathBuf {
        let width = self.options.zero_pad;
        let index = self.index;
        let name = if self.options.prefix.is_empty() {
            if width == 0 {
                format!("{index}.{ext}")
            } else {
                format!("{index:0width$}.{ext}")
            }
        } else if width == 0 {
            format!("{}_{}.{}", self.options.prefix, index, ext)
        } else {
            format!("{}_{index:0width$}.{ext}", self.options.prefix)
        };
        self.dir.join(name)
    }
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default()
}

fn write_session_metadata(
    path: &Path,
    metadata: &RecordingSessionMetadata,
) -> Result<(), RecordingError> {
    let mut file = File::create(path)?;
    writeln!(file, "{{")?;
    writeln!(
        file,
        "  \"session_id\": \"{}\",",
        json_escape(&metadata.session_id)
    )?;
    writeln!(file, "  \"started_unix_ms\": {},", metadata.started_unix_ms)?;
    writeln!(
        file,
        "  \"directory\": \"{}\",",
        json_escape(&metadata.directory.display().to_string())
    )?;
    writeln!(file, "  \"prefix\": \"{}\",", json_escape(&metadata.prefix))?;
    writeln!(file, "  \"format\": \"{:?}\"", metadata.format)?;
    writeln!(file, "}}")?;
    Ok(())
}

fn write_index_header(path: &Path) -> Result<(), RecordingError> {
    let mut file = File::create(path)?;
    writeln!(
        file,
        "sequence\ttimestamp\tfourcc\twidth\theight\tpayload_bytes\tpath"
    )?;
    Ok(())
}

fn append_index_entry(path: &Path, entry: &RecordingFrameIndexEntry) -> Result<(), RecordingError> {
    let mut file = OpenOptions::new().append(true).open(path)?;
    writeln!(
        file,
        "{}\t{}\t{}\t{}\t{}\t{}\t{}",
        entry.sequence,
        entry.timestamp,
        entry.fourcc,
        entry.width,
        entry.height,
        entry.payload_bytes,
        entry.path.display()
    )?;
    Ok(())
}

fn json_escape(value: &str) -> String {
    value
        .chars()
        .flat_map(|ch| match ch {
            '\\' => "\\\\".chars().collect::<Vec<_>>(),
            '"' => "\\\"".chars().collect::<Vec<_>>(),
            '\n' => "\\n".chars().collect::<Vec<_>>(),
            '\r' => "\\r".chars().collect::<Vec<_>>(),
            '\t' => "\\t".chars().collect::<Vec<_>>(),
            other => vec![other],
        })
        .collect()
}

fn save_frame_image(
    frame: &FrameLease,
    path: &Path,
    format: ImageFormat,
) -> Result<(), RecordingError> {
    let code = frame.meta().format.code;
    let save = |raw: &[u8], width: u32, height: u32, color: ColorType| {
        save_buffer_with_format(path, raw, width, height, color, format)
            .map_err(RecordingError::from)
    };

    let res = frame.meta().format.resolution;
    let width = res.width.get();
    let height = res.height.get();

    if matches!(code, FourCc::R8 | FourCc::GREY) {
        let bytes = packed_plane_bytes(frame, 1).ok_or(RecordingError::EmptyFrame)?;
        return save(&bytes, width, height, ColorType::L8);
    }

    if matches!(code, FourCc::RG24 | FourCc::RGB3) {
        let bytes = packed_plane_bytes(frame, 3).ok_or(RecordingError::EmptyFrame)?;
        return save(&bytes, width, height, ColorType::Rgb8);
    }

    if code == FourCc::RGBA {
        let bytes = packed_plane_bytes(frame, 4).ok_or(RecordingError::EmptyFrame)?;
        return save(&bytes, width, height, ColorType::Rgba8);
    }

    Err(RecordingError::UnsupportedFormat(code.to_string()))
}

fn packed_plane_bytes(frame: &FrameLease, bytes_per_pixel: usize) -> Option<Cow<'_, [u8]>> {
    let plane = frame.planes().into_iter().next()?;
    let res = frame.meta().format.resolution;
    let width_bytes = res.width.get() as usize * bytes_per_pixel;
    let height = res.height.get() as usize;
    let expected_len = width_bytes.checked_mul(height)?;
    let stride = plane.stride();
    let data = plane.data();
    if expected_len == 0 || data.is_empty() {
        return None;
    }
    if stride == width_bytes && data.len() >= expected_len {
        return Some(Cow::Borrowed(&data[..expected_len]));
    }
    if stride < width_bytes
        || data.len() < stride.saturating_mul(height.saturating_sub(1)) + width_bytes
    {
        return None;
    }
    let mut packed = Vec::with_capacity(expected_len);
    for y in 0..height {
        let row_start = y * stride;
        packed.extend_from_slice(&data[row_start..row_start + width_bytes]);
    }
    Some(Cow::Owned(packed))
}

fn write_encoded_frame(frame: &FrameLease, path: &Path) -> Result<(), RecordingError> {
    let planes = frame.planes();
    let plane = planes.first().ok_or(RecordingError::EmptyFrame)?;
    if plane.data().is_empty() {
        return Err(RecordingError::EmptyFrame);
    }
    let mut file = File::create(path)?;
    file.write_all(plane.data())?;
    Ok(())
}

fn is_jpeg_fourcc(code: FourCc) -> bool {
    code.is_jpeg_encoded()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;
    use styx_core::prelude::{
        BufferPool, ColorSpace, FrameMeta, MediaFormat, Resolution, plane_layout_from_dims,
    };

    #[test]
    fn recorder_writes_session_metadata_and_index() {
        let dir = std::env::temp_dir().join(format!(
            "styx-recorder-index-{}-{}",
            std::process::id(),
            now_unix_ms()
        ));
        let mut recorder = FrameRecorder::new(
            &dir,
            RecordingOptions {
                prefix: "test".into(),
                format: RecordingFormat::Png,
                session_id: Some("session-a".into()),
                ..Default::default()
            },
        )
        .expect("recorder");
        let frame = test_rg24_frame(2, 2, 9);
        recorder.record(&frame).expect("record frame");

        let metadata = std::fs::read_to_string(dir.join("session.json")).expect("metadata");
        let index = std::fs::read_to_string(dir.join("index.tsv")).expect("index");
        assert!(metadata.contains("\"session_id\": \"session-a\""));
        assert!(index.contains("sequence\ttimestamp\tfourcc"));
        assert!(index.contains("0\t9\tRG24\t2\t2\t12"));
        assert_eq!(recorder.metadata().session_id, "session-a");
        assert_eq!(recorder.paths().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn packed_plane_bytes_removes_row_padding() {
        let frame = test_rg24_frame_with_stride(2, 2, 8, 1);
        let packed = packed_plane_bytes(&frame, 3).expect("packed bytes");
        assert_eq!(&*packed, &[0, 1, 2, 3, 4, 5, 8, 9, 10, 11, 12, 13]);
    }

    fn test_rg24_frame(width: u32, height: u32, timestamp: u64) -> FrameLease {
        let res = Resolution::new(width, height).expect("resolution");
        let layout = plane_layout_from_dims(
            NonZeroU32::new(width).unwrap(),
            NonZeroU32::new(height).unwrap(),
            3,
        );
        let pool = BufferPool::with_limits(1, layout.len, 1);
        let mut lease = pool.lease();
        lease.resize(layout.len);
        for (idx, byte) in lease.as_mut_slice().iter_mut().enumerate() {
            *byte = idx as u8;
        }
        FrameLease::single_plane(
            FrameMeta::new(
                MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb),
                timestamp,
            ),
            lease,
            layout.len,
            layout.stride,
        )
    }

    fn test_rg24_frame_with_stride(
        width: u32,
        height: u32,
        stride: usize,
        timestamp: u64,
    ) -> FrameLease {
        let res = Resolution::new(width, height).expect("resolution");
        let len = stride * height as usize;
        let pool = BufferPool::with_limits(1, len, 1);
        let mut lease = pool.lease();
        lease.resize(len);
        for (idx, byte) in lease.as_mut_slice().iter_mut().enumerate() {
            *byte = idx as u8;
        }
        FrameLease::single_plane(
            FrameMeta::new(
                MediaFormat::new(FourCc::RG24, res, ColorSpace::Srgb),
                timestamp,
            ),
            lease,
            len,
            stride,
        )
    }
}
