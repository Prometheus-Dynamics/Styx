//! Minimal preview window for examples (feature `preview-window`).

use std::time::Duration;

use minifb::{Window, WindowOptions};
use styx_core::prelude::*;

/// Simple RGBA/RGB preview window backed by minifb.
///
/// # Example
/// ```rust,ignore
/// use styx::prelude::*;
///
/// let mut window = PreviewWindow::new("preview", 640, 480)?;
/// # Ok::<(), minifb::Error>(())
/// ```
pub struct PreviewWindow {
    window: Window,
    buffer: Vec<u32>,
    width: usize,
    height: usize,
}

impl PreviewWindow {
    /// Create a preview window with the given initial size.
    pub fn new(title: &str, width: u32, height: u32) -> Result<Self, minifb::Error> {
        let width = width.max(1) as usize;
        let height = height.max(1) as usize;
        let mut window = Window::new(title, width, height, WindowOptions::default())?;
        window.limit_update_rate(Some(Duration::from_millis(16)));
        Ok(Self {
            window,
            buffer: vec![0; width * height],
            width,
            height,
        })
    }

    /// Display a frame; supports RG24/RGBA/BGR3/BGRA planes.
    ///
    /// Returns an error if the window is closed or the format is unsupported.
    pub fn show(&mut self, frame: &FrameLease) -> Result<(), String> {
        if !self.window.is_open() {
            return Err("preview window closed".into());
        }
        let meta = frame.meta();
        let width = meta.format.resolution.width.get() as usize;
        let height = meta.format.resolution.height.get() as usize;
        if width == 0 || height == 0 {
            return Err("invalid frame resolution".into());
        }
        if width != self.width || height != self.height {
            self.width = width;
            self.height = height;
            self.buffer.resize(width * height, 0);
        }

        let planes = frame.planes();
        let Some(plane) = planes.get(0) else {
            return Err("frame missing plane".into());
        };
        let code = meta.format.code;
        if code == FourCc::new(*b"RG24") {
            self.blit_rgb24(plane, false)?;
        } else if code == FourCc::new(*b"BGR3") {
            self.blit_rgb24(plane, true)?;
        } else if code == FourCc::new(*b"RGBA") {
            self.blit_rgba(plane, false)?;
        } else if code == FourCc::new(*b"BGRA") {
            self.blit_rgba(plane, true)?;
        } else {
            return Err(format!("unsupported preview format {:?}", code));
        }

        self.window
            .update_with_buffer(&self.buffer, self.width, self.height)
            .map_err(|e| e.to_string())
    }

    fn blit_rgb24(&mut self, plane: &Plane<'_>, swap_rb: bool) -> Result<(), String> {
        let stride = plane.stride().max(self.width.saturating_mul(3));
        let data = plane.data();
        let required = stride
            .checked_mul(self.height)
            .ok_or_else(|| "frame stride overflow".to_string())?;
        if data.len() < required {
            return Err("frame too small for rgb24".into());
        }
        for y in 0..self.height {
            let row = &data[y * stride..];
            for x in 0..self.width {
                let i = x * 3;
                if i + 3 > row.len() {
                    break;
                }
                let (r, g, b) = (row[i], row[i + 1], row[i + 2]);
                let (r, b) = if swap_rb { (b, r) } else { (r, b) };
                self.buffer[y * self.width + x] =
                    (0xFF << 24) | (r as u32) << 16 | (g as u32) << 8 | (b as u32);
            }
        }
        Ok(())
    }

    fn blit_rgba(&mut self, plane: &Plane<'_>, swap_rb: bool) -> Result<(), String> {
        let stride = plane.stride().max(self.width.saturating_mul(4));
        let data = plane.data();
        let required = stride
            .checked_mul(self.height)
            .ok_or_else(|| "frame stride overflow".to_string())?;
        if data.len() < required {
            return Err("frame too small for rgba".into());
        }
        for y in 0..self.height {
            let row = &data[y * stride..];
            for x in 0..self.width {
                let i = x * 4;
                if i + 4 > row.len() {
                    break;
                }
                let (r, g, b, a) = (row[i], row[i + 1], row[i + 2], row[i + 3]);
                let (r, b) = if swap_rb { (b, r) } else { (r, b) };
                self.buffer[y * self.width + x] =
                    (a as u32) << 24 | (r as u32) << 16 | (g as u32) << 8 | (b as u32);
            }
        }
        Ok(())
    }
}
