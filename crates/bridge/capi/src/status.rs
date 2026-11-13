//! Status codes, the failure record, and the guard every entry point runs in.
//!
//! Every entry point returns one status. A failure carries a message, kept per thread for
//! `styx_last_error`. A panic inside an entry point is caught here and becomes `STYX_E_PANIC`:
//! no unwind crosses the C boundary.

use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};

/// The call succeeded.
pub const OK: i32 = 0;
/// A NULL pointer, a length or index out of range, bad text, or a value the control does not take.
pub const E_INVALID: i32 = -1;
/// No such camera, no such control, or no frame has arrived yet.
pub const E_NOT_FOUND: i32 = -2;
/// A socket, the Lemnos daemon or a device call failed.
pub const E_IO: i32 = -3;
/// No frame arrived within the timeout.
pub const E_TIMEOUT: i32 = -4;
/// This path or backend does not do what was asked.
pub const E_UNSUPPORTED: i32 = -5;
/// The camera is owned or in use elsewhere, or is not publishing yet.
pub const E_BUSY: i32 = -6;
/// The other end answered in a form this build does not understand.
pub const E_PROTOCOL: i32 = -7;
/// An internal failure, caught at the boundary.
pub const E_PANIC: i32 = -8;
/// The camera stopped, faulted, or its service connection ended. Open it again.
pub const E_STOPPED: i32 = -9;
/// A control value outside the control's range.
pub const E_RANGE: i32 = -10;

/// A failed call: its status and a message for `styx_last_error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fail {
    pub(crate) status: i32,
    pub(crate) message: String,
}

pub(crate) type Res<T> = Result<T, Fail>;

impl Fail {
    pub(crate) fn new(status: i32, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(E_INVALID, message)
    }

    pub(crate) fn io(message: impl std::fmt::Display) -> Self {
        Self::new(E_IO, message.to_string())
    }
}

thread_local! {
    static LAST: RefCell<String> = const { RefCell::new(String::new()) };
}

/// The message of this thread's last failed call (empty when none has failed).
pub(crate) fn last_error() -> String {
    LAST.try_with(|last| last.borrow().clone())
        .unwrap_or_default()
}

/// Runs an entry point's body. Returns its status; a failure's message is kept for
/// `styx_last_error`; a panic is `E_PANIC`.
pub(crate) fn guard(body: impl FnOnce() -> Res<()>) -> i32 {
    let (status, message) = match catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(())) => return OK,
        Ok(Err(fail)) => (fail.status, fail.message),
        Err(_) => (
            E_PANIC,
            "internal failure, caught at the C boundary".to_string(),
        ),
    };
    let _ = LAST.try_with(|last| *last.borrow_mut() = message);
    status
}

/// The longest name or path a caller may pass.
pub(crate) const MAX_TEXT: usize = 4096;

/// A caller's bytes. Zero length is empty whatever the pointer; a pointer that is NULL with a
/// length, or a length no slice can have, is refused.
///
/// # Safety
///
/// If `len` is not zero, `ptr` must point to `len` readable bytes that stay unchanged while the
/// result is used. The caller of the C entry point promises this; a NULL or a length past the
/// caller's own buffer is refused here, but a dangling pointer is not detectable.
pub(crate) unsafe fn bytes<'a>(ptr: *const u8, len: usize) -> Res<&'a [u8]> {
    if len == 0 {
        return Ok(&[]);
    }
    if ptr.is_null() {
        return Err(Fail::invalid("NULL with a non-zero length"));
    }
    if len > isize::MAX as usize {
        return Err(Fail::invalid("a length past the largest allowed"));
    }
    // SAFETY: non-NULL, the length is within isize::MAX, and the entry point's contract (see
    // this function's `# Safety`) makes `ptr..ptr+len` readable for the result's lifetime.
    Ok(unsafe { std::slice::from_raw_parts(ptr, len) })
}

/// A caller's UTF-8 text, at most [`MAX_TEXT`] bytes.
///
/// # Safety
///
/// As [`bytes`].
pub(crate) unsafe fn text<'a>(ptr: *const u8, len: usize) -> Res<&'a str> {
    if len > MAX_TEXT {
        return Err(Fail::invalid(format!("text longer than {MAX_TEXT} bytes")));
    }
    // SAFETY: forwarded to `bytes` under the same contract.
    let raw = unsafe { bytes(ptr, len)? };
    std::str::from_utf8(raw).map_err(|_| Fail::invalid("text is not UTF-8"))
}

/// Writes `value` through an out-pointer.
///
/// # Safety
///
/// `out` must be NULL or point to writable memory for a `T`.
pub(crate) unsafe fn put<T>(out: *mut T, value: T) -> Res<()> {
    if out.is_null() {
        return Err(Fail::invalid("NULL output pointer"));
    }
    // SAFETY: non-NULL, and writable for a `T` by the entry point's contract.
    unsafe { out.write(value) };
    Ok(())
}

/// Refuses a caller's output buffer before any work is done: a NULL length, or a NULL buffer with
/// a capacity, or a capacity no slice can have.
pub(crate) fn check_text_out(buf: *mut u8, cap: usize, len: *mut usize) -> Res<()> {
    if len.is_null() {
        return Err(Fail::invalid("NULL length output"));
    }
    if cap > 0 && buf.is_null() {
        return Err(Fail::invalid("NULL buffer with a capacity"));
    }
    if cap > isize::MAX as usize {
        return Err(Fail::invalid("a capacity past the largest allowed"));
    }
    Ok(())
}

/// Writes `text` into a caller's buffer of `cap` bytes and sets `*len` to its full length. The
/// buffer gets at most `cap - 1` bytes and a NUL when `cap` is not zero: a caller finds a cut
/// text by comparing `*len` with `cap`.
///
/// # Safety
///
/// `buf` must be NULL (only when `cap` is zero) or point to `cap` writable bytes; `len` must be
/// NULL or point to a writable `usize`.
pub(crate) unsafe fn write_bytes(
    text: &[u8],
    buf: *mut u8,
    cap: usize,
    len: *mut usize,
) -> Res<()> {
    check_text_out(buf, cap, len)?;
    // SAFETY: `len` is non-NULL and writable by the contract.
    unsafe { len.write(text.len()) };
    if cap > 0 {
        let n = text.len().min(cap - 1);
        // SAFETY: `buf` has `cap` writable bytes and `n + 1 <= cap`; `text` is `text.len() >= n`
        // readable bytes that do not overlap the caller's buffer.
        unsafe {
            std::ptr::copy_nonoverlapping(text.as_ptr(), buf, n);
            buf.add(n).write(0);
        }
    }
    Ok(())
}

/// A handle from a caller, borrowed.
///
/// # Safety
///
/// `ptr` must be NULL or a handle this library returned and has not released.
pub(crate) unsafe fn handle<'a, T>(ptr: *const T) -> Res<&'a T> {
    if ptr.is_null() {
        return Err(Fail::invalid("NULL handle"));
    }
    // SAFETY: non-NULL, and a live handle from this library by the contract.
    Ok(unsafe { &*ptr })
}

/// A handle from a caller, borrowed mutably.
///
/// # Safety
///
/// As [`handle`], and no other reference to the handle is in use.
pub(crate) unsafe fn handle_mut<'a, T>(ptr: *mut T) -> Res<&'a mut T> {
    if ptr.is_null() {
        return Err(Fail::invalid("NULL handle"));
    }
    // SAFETY: non-NULL, a live handle by the contract, and not otherwise borrowed.
    Ok(unsafe { &mut *ptr })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_becomes_a_status_and_a_failure_keeps_its_message() {
        assert_eq!(guard(|| panic!("boom")), E_PANIC);
        assert!(last_error().contains("caught"));
        assert_eq!(
            guard(|| Err(Fail::new(E_NOT_FOUND, "no such camera"))),
            E_NOT_FOUND
        );
        assert_eq!(last_error(), "no such camera");
        assert_eq!(guard(|| Ok(())), OK);
    }

    #[test]
    fn text_is_cut_and_nul_terminated_and_the_full_length_is_reported() {
        let mut buf = [0xff_u8; 4];
        let mut len = 0_usize;
        // SAFETY: `buf` has 4 writable bytes and `len` is a valid usize.
        unsafe { write_bytes(b"abcdef", buf.as_mut_ptr(), buf.len(), &mut len) }.expect("write");
        assert_eq!(len, 6);
        assert_eq!(&buf, b"abc\0");
        // SAFETY: a zero capacity with a NULL buffer writes only the length.
        unsafe { write_bytes(b"xy", std::ptr::null_mut(), 0, &mut len) }.expect("length only");
        assert_eq!(len, 2);
    }
}
