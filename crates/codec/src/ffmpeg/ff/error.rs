use super::*;

/// `EAGAIN`, as reported in [`Error::Other`].
pub const EAGAIN: c_int = libc::EAGAIN;

/// An FFmpeg error code, or FFmpeg being unavailable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    Eof,
    /// `AVERROR(errno)` for a POSIX errno.
    Other {
        errno: c_int,
    },
    /// Any other FFmpeg error code.
    Code(c_int),
    /// The FFmpeg libraries could not be loaded.
    Unavailable(String),
    InvalidData,
}

impl From<c_int> for Error {
    fn from(code: c_int) -> Self {
        if code == raw::AVERROR_EOF {
            Error::Eof
        } else if (-4095..0).contains(&code) {
            Error::Other { errno: -code }
        } else {
            Error::Code(code)
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = match self {
            Error::Unavailable(msg) => return f.write_str(msg),
            Error::InvalidData => return f.write_str("invalid data"),
            Error::Eof => raw::AVERROR_EOF,
            Error::Other { errno } => -errno,
            Error::Code(code) => *code,
        };
        let mut buf = [0 as std::ffi::c_char; 128];
        match loader::core() {
            // SAFETY: buffer of the given size; av_strerror NUL-terminates.
            Ok(core)
                if unsafe { (core.util.av_strerror)(code, buf.as_mut_ptr(), buf.len()) } == 0 =>
            {
                let msg = unsafe { CStr::from_ptr(buf.as_ptr()) };
                f.write_str(&msg.to_string_lossy())
            }
            _ => write!(f, "ffmpeg error {code}"),
        }
    }
}

impl std::error::Error for Error {}

pub(crate) fn check(code: c_int) -> Result<c_int, Error> {
    if code < 0 {
        Err(Error::from(code))
    } else {
        Ok(code)
    }
}
