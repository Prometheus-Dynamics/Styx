//! Unix `SOCK_SEQPACKET` sockets carrying file descriptors (`SCM_RIGHTS`). Message boundaries
//! are kept, so each frame is one message with its descriptors attached. The frame socket
//! ([`super::frame_socket`]) uses `SOCK_STREAM` sockets, as the transport it speaks does.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::path::Path;
use std::time::Duration;

/// Largest message: a frame header with its planes.
pub(super) const MAX_MESSAGE: usize = 8192;
/// Most descriptors one message carries (one per plane, for a frame and its companions).
const MAX_FDS: usize = 16;

pub(super) enum Received {
    Message(Vec<u8>, Vec<OwnedFd>),
    /// Nothing arrived in time.
    Nothing,
    /// The other end closed the connection.
    Closed,
}

fn check(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

fn unix_socket(kind: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: plain socket creation; a non-negative result is a new descriptor we own.
    let fd = check(unsafe { libc::socket(libc::AF_UNIX, kind | libc::SOCK_CLOEXEC, 0) })?;
    // SAFETY: `fd` was just created and is owned by nobody else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn address(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    // SAFETY: `sockaddr_un` is plain data; all-zero is a valid value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path is too long",
        ));
    }
    for (dst, &src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = src as libc::c_char;
    }
    let len = std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    Ok((addr, len as libc::socklen_t))
}

/// A non-blocking listening socket at `path`; a stale socket file there is replaced.
pub(super) fn listen(path: &Path) -> io::Result<OwnedFd> {
    listen_kind(path, libc::SOCK_SEQPACKET)
}

/// [`listen`] for a `SOCK_STREAM` socket.
#[cfg(feature = "frame-socket")]
pub(super) fn listen_stream(path: &Path) -> io::Result<OwnedFd> {
    listen_kind(path, libc::SOCK_STREAM)
}

fn listen_kind(path: &Path, kind: libc::c_int) -> io::Result<OwnedFd> {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket()) {
        std::fs::remove_file(path)?;
    }
    let socket = unix_socket(kind)?;
    let (addr, len) = address(path)?;
    // SAFETY: `addr` is a valid `sockaddr_un` of `len` bytes; the fd is open.
    check(unsafe {
        libc::bind(
            socket.as_raw_fd(),
            (&raw const addr).cast::<libc::sockaddr>(),
            len,
        )
    })?;
    // SAFETY: the fd is an open, bound socket.
    check(unsafe { libc::listen(socket.as_raw_fd(), 16) })?;
    // SAFETY: setting a status flag on an open fd.
    check(unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) })?;
    Ok(socket)
}

/// A pending connection, if any (the listener is non-blocking).
pub(super) fn accept(listener: &OwnedFd) -> io::Result<Option<OwnedFd>> {
    // SAFETY: accepting on an open listening socket; no peer address is requested.
    let fd = unsafe {
        libc::accept4(
            listener.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
        )
    };
    if fd < 0 {
        let err = io::Error::last_os_error();
        return match err.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(None),
            _ => Err(err),
        };
    }
    // SAFETY: `accept4` returned a new descriptor we own.
    Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }))
}

pub(super) fn connect(path: &Path) -> io::Result<OwnedFd> {
    connect_kind(path, libc::SOCK_SEQPACKET)
}

/// [`connect`] for a `SOCK_STREAM` socket.
#[cfg(feature = "frame-socket")]
pub(super) fn connect_stream(path: &Path) -> io::Result<OwnedFd> {
    connect_kind(path, libc::SOCK_STREAM)
}

fn connect_kind(path: &Path, kind: libc::c_int) -> io::Result<OwnedFd> {
    let socket = unix_socket(kind)?;
    let (addr, len) = address(path)?;
    // SAFETY: `addr` is a valid `sockaddr_un` of `len` bytes; the fd is open.
    check(unsafe {
        libc::connect(
            socket.as_raw_fd(),
            (&raw const addr).cast::<libc::sockaddr>(),
            len,
        )
    })?;
    Ok(socket)
}

/// Send one message with `fds` attached, without blocking. `Ok(false)`: the socket is full.
pub(super) fn send(socket: &OwnedFd, bytes: &[u8], fds: &[RawFd]) -> io::Result<bool> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr().cast_mut().cast(),
        iov_len: bytes.len(),
    };
    let fd_bytes = std::mem::size_of_val(fds) as u32;
    // SAFETY: `CMSG_SPACE` is a pure size computation.
    let space = if fds.is_empty() {
        0
    } else {
        unsafe { libc::CMSG_SPACE(fd_bytes) as usize }
    };
    // u64 elements keep the control buffer aligned for `cmsghdr`.
    let mut control = vec![0u64; space.div_ceil(8)];
    // SAFETY: `msghdr` is plain data; all-zero is a valid value.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        // SAFETY: the control buffer holds `CMSG_SPACE(fd_bytes)` bytes, so the first header
        // and its data (`fd_bytes`) are in bounds.
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&msg);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(fd_bytes) as _;
            std::ptr::copy_nonoverlapping(
                fds.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(header),
                fd_bytes as usize,
            );
        }
    }
    // SAFETY: `msg` points at live buffers for the duration of the call.
    let sent = unsafe {
        libc::sendmsg(
            socket.as_raw_fd(),
            &msg,
            libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
        )
    };
    if sent < 0 {
        let err = io::Error::last_os_error();
        return match err.kind() {
            io::ErrorKind::WouldBlock => Ok(false),
            _ => Err(err),
        };
    }
    // Only a stream socket short of room writes part of a message.
    if sent as usize != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "message only partly sent",
        ));
    }
    Ok(true)
}

/// Which of `fds` are readable (or closed by the other end), waiting up to `wait` for any.
#[cfg(feature = "frame-socket")]
pub(super) fn poll_readable(fds: &[RawFd], wait: Duration) -> io::Result<Vec<bool>> {
    let mut polls: Vec<libc::pollfd> = fds
        .iter()
        .map(|&fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();
    let timeout = i32::try_from(wait.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: `polls` holds `polls.len()` valid `pollfd`s for the duration of the call.
    let ready = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, timeout) };
    if ready < 0 {
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
    Ok(polls.iter().map(|p| p.revents != 0).collect())
}

/// The process at the other end of a connected Unix socket, as the kernel reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerCredentials {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

pub(super) fn peer_credentials(socket: &OwnedFd) -> io::Result<PeerCredentials> {
    // SAFETY: `ucred` is plain data; all-zero is a valid value.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` is writable for `len` bytes; the fd is an open socket.
    check(unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    })?;
    Ok(PeerCredentials {
        pid: cred.pid,
        uid: cred.uid,
        gid: cred.gid,
    })
}

/// Whether `socket` becomes readable (or a listener has a connection) within `wait`.
pub(super) fn readable(socket: &OwnedFd, wait: Duration) -> bool {
    let mut poll = libc::pollfd {
        fd: socket.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout = i32::try_from(wait.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: one valid `pollfd`.
    unsafe { libc::poll(&mut poll, 1, timeout) > 0 }
}

/// Receive one message and its descriptors, waiting up to `wait` (zero: without waiting).
pub(super) fn recv(socket: &OwnedFd, wait: Duration) -> io::Result<Received> {
    let mut poll = libc::pollfd {
        fd: socket.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout = i32::try_from(wait.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: one valid `pollfd`.
    let ready = unsafe { libc::poll(&mut poll, 1, timeout) };
    if ready < 0 {
        let err = io::Error::last_os_error();
        return match err.kind() {
            io::ErrorKind::Interrupted => Ok(Received::Nothing),
            _ => Err(err),
        };
    }
    if ready == 0 {
        return Ok(Received::Nothing);
    }
    let mut bytes = vec![0u8; MAX_MESSAGE];
    let mut iov = libc::iovec {
        iov_base: bytes.as_mut_ptr().cast(),
        iov_len: bytes.len(),
    };
    // SAFETY: `CMSG_SPACE` is a pure size computation.
    let space =
        unsafe { libc::CMSG_SPACE((MAX_FDS * std::mem::size_of::<RawFd>()) as u32) as usize };
    let mut control = vec![0u64; space.div_ceil(8)];
    // SAFETY: `msghdr` is plain data; all-zero is a valid value.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: `msg` points at live buffers for the duration of the call.
    let len = unsafe {
        libc::recvmsg(
            socket.as_raw_fd(),
            &mut msg,
            libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC,
        )
    };
    if len < 0 {
        let err = io::Error::last_os_error();
        return match err.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(Received::Nothing),
            io::ErrorKind::ConnectionReset => Ok(Received::Closed),
            _ => Err(err),
        };
    }
    // Descriptors first, so they are owned (and closed) whatever happens next.
    let mut fds = Vec::new();
    // SAFETY: walking the control headers `recvmsg` filled in, within `msg_controllen`.
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&msg);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let data_len = (*header).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let data = libc::CMSG_DATA(header);
                for i in 0..data_len / std::mem::size_of::<RawFd>() {
                    let fd = std::ptr::read_unaligned(data.cast::<RawFd>().add(i));
                    fds.push(OwnedFd::from_raw_fd(fd));
                }
            }
            header = libc::CMSG_NXTHDR(&msg, header);
        }
    }
    if len == 0 && fds.is_empty() {
        return Ok(Received::Closed);
    }
    if msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message or descriptors truncated",
        ));
    }
    bytes.truncate(len as usize);
    Ok(Received::Message(bytes, fds))
}
