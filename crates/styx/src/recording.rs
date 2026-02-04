use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use image::{DynamicImage, ImageFormat};
use styx_codec::decoder::frame_to_dynamic_image;
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
}

impl Default for RecordingOptions {
    fn default() -> Self {
        Self {
            prefix: "frame".into(),
            format: RecordingFormat::Auto,
            zero_pad: 6,
            start_index: 0,
        }
    }
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
/// ```rust,ignore
/// use styx::prelude::*;
///
/// let recorder = FrameRecorder::new("./recordings", RecordingOptions::default())?;
/// let mut pipeline = MediaPipelineBuilder::new(CaptureRequest::new(&device))
///     .record_output(recorder)
///     .start()?;
///
/// while let RecvOutcome::Data(_) = pipeline.next() {}
/// let recorder = pipeline.stop_with_recorder().expect("recorder");
/// let replay = make_file_device("replay", recorder.into_paths(), 30, true);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct FrameRecorder {
    dir: PathBuf,
    options: RecordingOptions,
    index: u64,
    paths: Vec<PathBuf>,
    error_count: usize,
    last_error: Option<String>,
}

impl FrameRecorder {
    /// Create a recorder that writes numbered image files to `dir`.
    pub fn new(dir: impl Into<PathBuf>, options: RecordingOptions) -> Result<Self, RecordingError> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            index: options.start_index,
            options,
            paths: Vec::new(),
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
                    let img = frame_to_image(frame, code)?;
                    img.save_with_format(&path, ImageFormat::Png)?;
                }
            }
            RecordingFormat::Png => {
                let img = frame_to_image(frame, code)?;
                img.save_with_format(&path, ImageFormat::Png)?;
            }
            RecordingFormat::Jpeg => {
                if is_jpeg_fourcc(code) {
                    write_encoded_frame(frame, &path)?;
                } else {
                    let img = frame_to_image(frame, code)?;
                    img.save_with_format(&path, ImageFormat::Jpeg)?;
                }
            }
        }

        self.paths.push(path.clone());
        self.index = self.index.saturating_add(1);
        Ok(path)
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

fn frame_to_image(frame: &FrameLease, code: FourCc) -> Result<DynamicImage, RecordingError> {
    if let Some(img) = frame_to_dynamic_image(frame) {
        return Ok(img);
    }
    if is_jpeg_fourcc(code) {
        let planes = frame.planes();
        let plane = planes.first().ok_or(RecordingError::EmptyFrame)?;
        if plane.data().is_empty() {
            return Err(RecordingError::EmptyFrame);
        }
        return Ok(image::load_from_memory(plane.data())?);
    }
    Err(RecordingError::UnsupportedFormat(code.to_string()))
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
    code == FourCc::new(*b"MJPG") || code == FourCc::new(*b"JPEG")
}
