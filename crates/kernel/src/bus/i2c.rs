//! I²C through the i2c-dev character devices (`/dev/i2c-N`).
//!
//! A [`I2cDevice`] is one target address on one bus. Opening it claims the address with
//! `I2C_SLAVE` — never `I2C_SLAVE_FORCE` — so it fails with `EBUSY` while a kernel driver owns
//! that address. Register accesses are combined `I2C_RDWR` transactions: a read is one write
//! message carrying the register address and one read message, joined by a repeated start.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use super::ioctl::{ioctl, ioctl_int};

const I2C_SLAVE: u32 = 0x0703;
const I2C_FUNCS: u32 = 0x0705;
const I2C_RDWR: u32 = 0x0707;

const I2C_M_RD: u16 = 0x0001;
const I2C_FUNC_I2C: libc::c_ulong = 0x0000_0001;

/// Most messages the kernel accepts in one `I2C_RDWR` call (`I2C_RDWR_IOCTL_MAX_MSGS`).
pub const MAX_MESSAGES: usize = 42;
/// Longest message the kernel accepts (`8192` bytes).
pub const MAX_MESSAGE_LEN: usize = 8192;

/// `struct i2c_msg`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct I2cMsg {
    pub addr: u16,
    pub flags: u16,
    pub len: u16,
    pub buf: *mut u8,
}

/// `struct i2c_rdwr_ioctl_data`.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct I2cRdwrData {
    pub msgs: *mut I2cMsg,
    pub nmsgs: u32,
}

/// Width of a register address on the wire (sent most significant byte first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddrWidth {
    /// One byte.
    Bits8,
    /// Two bytes, big-endian (most camera sensors).
    Bits16,
}

impl AddrWidth {
    /// Bytes on the wire.
    pub const fn bytes(self) -> usize {
        match self {
            AddrWidth::Bits8 => 1,
            AddrWidth::Bits16 => 2,
        }
    }
}

/// Width of a register value on the wire (big-endian, as camera sensors use).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueWidth {
    /// One byte.
    Bits8,
    /// Two bytes.
    Bits16,
    /// Four bytes.
    Bits32,
}

impl ValueWidth {
    /// Bytes on the wire.
    pub const fn bytes(self) -> usize {
        match self {
            ValueWidth::Bits8 => 1,
            ValueWidth::Bits16 => 2,
            ValueWidth::Bits32 => 4,
        }
    }

    const fn max(self) -> u32 {
        match self {
            ValueWidth::Bits8 => 0xff,
            ValueWidth::Bits16 => 0xffff,
            ValueWidth::Bits32 => u32::MAX,
        }
    }
}

/// Encodes a register address into the start of `out`; returns the bytes written.
pub fn encode_reg_addr(reg: u16, width: AddrWidth, out: &mut [u8]) -> io::Result<usize> {
    match width {
        AddrWidth::Bits8 => {
            let b = u8::try_from(reg)
                .map_err(|_| invalid(format!("register {reg:#x} does not fit 8 bits")))?;
            out[0] = b;
            Ok(1)
        }
        AddrWidth::Bits16 => {
            out[..2].copy_from_slice(&reg.to_be_bytes());
            Ok(2)
        }
    }
}

/// Encodes a register write (address then value, big-endian) into a fixed buffer; returns the
/// buffer and the length used.
pub fn encode_write(
    reg: u16,
    addr: AddrWidth,
    value: u32,
    width: ValueWidth,
) -> io::Result<([u8; 6], usize)> {
    if value > width.max() {
        return Err(invalid(format!(
            "value {value:#x} does not fit {} bytes",
            width.bytes()
        )));
    }
    let mut buf = [0u8; 6];
    let n = encode_reg_addr(reg, addr, &mut buf)?;
    let bytes = value.to_be_bytes();
    buf[n..n + width.bytes()].copy_from_slice(&bytes[4 - width.bytes()..]);
    Ok((buf, n + width.bytes()))
}

/// Decodes a big-endian register value.
pub fn decode_value(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0u32, |acc, b| (acc << 8) | u32::from(*b))
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

fn msg_len(len: usize) -> io::Result<u16> {
    if len == 0 || len > MAX_MESSAGE_LEN {
        return Err(invalid(format!(
            "I2C message of {len} bytes (1..={MAX_MESSAGE_LEN})"
        )));
    }
    Ok(len as u16)
}

/// One message of a combined transfer.
#[derive(Debug)]
pub enum Message<'a> {
    /// Write these bytes.
    Write(&'a [u8]),
    /// Read into this buffer.
    Read(&'a mut [u8]),
}

/// Builds the kernel message array for `messages` to `addr`. The returned structs borrow the
/// buffers in `messages` through raw pointers; keep `messages` alive and unmoved while they are
/// used.
pub(crate) fn build_messages(addr: u16, messages: &mut [Message<'_>]) -> io::Result<Vec<I2cMsg>> {
    if messages.is_empty() || messages.len() > MAX_MESSAGES {
        return Err(invalid(format!(
            "{} messages in one transfer (1..={MAX_MESSAGES})",
            messages.len()
        )));
    }
    messages
        .iter_mut()
        .map(|m| match m {
            Message::Write(data) => Ok(I2cMsg {
                addr,
                flags: 0,
                len: msg_len(data.len())?,
                // The kernel only reads write buffers.
                buf: data.as_ptr().cast_mut(),
            }),
            Message::Read(buf) => Ok(I2cMsg {
                addr,
                flags: I2C_M_RD,
                len: msg_len(buf.len())?,
                buf: buf.as_mut_ptr(),
            }),
        })
        .collect()
}

/// One target address on an I²C bus, opened through i2c-dev.
#[derive(Debug)]
pub struct I2cDevice {
    file: File,
    path: PathBuf,
    addr: u16,
    addr_width: AddrWidth,
}

impl I2cDevice {
    /// Opens `/dev/i2c-{bus}` and claims 7-bit address `addr`.
    pub fn open(bus: u32, addr: u16, addr_width: AddrWidth) -> io::Result<Self> {
        Self::open_path(format!("/dev/i2c-{bus}"), addr, addr_width)
    }

    /// Opens an i2c-dev node by path and claims 7-bit address `addr`.
    ///
    /// Fails with `EBUSY` if a kernel driver is bound to the address, and with `Unsupported` if
    /// the adapter cannot do plain I²C transfers.
    pub fn open_path(path: impl AsRef<Path>, addr: u16, addr_width: AddrWidth) -> io::Result<Self> {
        if addr > 0x7f {
            return Err(invalid(format!("7-bit I2C address {addr:#x} out of range")));
        }
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(&path)?;

        let mut funcs: libc::c_ulong = 0;
        // SAFETY: I2C_FUNCS writes one `unsigned long` through the pointer, which is live.
        unsafe { ioctl(file.as_fd(), I2C_FUNCS, &mut funcs) }?;
        if funcs & I2C_FUNC_I2C == 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("{} cannot do I2C transfers", path.display()),
            ));
        }
        // Claims the address (EBUSY if a kernel driver owns it). Never I2C_SLAVE_FORCE.
        ioctl_int(file.as_fd(), I2C_SLAVE, libc::c_ulong::from(addr))?;
        Ok(Self {
            file,
            path,
            addr,
            addr_width,
        })
    }

    /// The device node this was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The 7-bit target address.
    pub fn address(&self) -> u16 {
        self.addr
    }

    /// Register address width.
    pub fn addr_width(&self) -> AddrWidth {
        self.addr_width
    }

    /// Runs `messages` as one combined transfer (repeated starts, one stop).
    pub fn transfer(&self, messages: &mut [Message<'_>]) -> io::Result<()> {
        let mut msgs = build_messages(self.addr, messages)?;
        let mut data = I2cRdwrData {
            msgs: msgs.as_mut_ptr(),
            nmsgs: msgs.len() as u32,
        };
        // SAFETY: `data` points at `msgs`, whose buffers point into `messages`; all stay alive
        // and unmoved for the duration of the call. Read buffers are exclusively borrowed.
        let done = unsafe { ioctl(self.file.as_fd(), I2C_RDWR, &mut data) }?;
        if done as usize != msgs.len() {
            return Err(io::Error::other(format!(
                "I2C transfer completed {done} of {} messages",
                msgs.len()
            )));
        }
        Ok(())
    }

    /// Reads `buf.len()` bytes starting at register `reg` (auto-incrementing).
    pub fn read_burst(&self, reg: u16, buf: &mut [u8]) -> io::Result<()> {
        let mut addr = [0u8; 2];
        let n = encode_reg_addr(reg, self.addr_width, &mut addr)?;
        self.transfer(&mut [Message::Write(&addr[..n]), Message::Read(buf)])
    }

    /// Reads a register of the given width.
    pub fn read_reg(&self, reg: u16, width: ValueWidth) -> io::Result<u32> {
        let mut buf = [0u8; 4];
        self.read_burst(reg, &mut buf[..width.bytes()])?;
        Ok(decode_value(&buf[..width.bytes()]))
    }

    /// Reads an 8-bit register.
    pub fn read8(&self, reg: u16) -> io::Result<u8> {
        self.read_reg(reg, ValueWidth::Bits8).map(|v| v as u8)
    }

    /// Reads a 16-bit register.
    pub fn read16(&self, reg: u16) -> io::Result<u16> {
        self.read_reg(reg, ValueWidth::Bits16).map(|v| v as u16)
    }

    /// Reads a 32-bit register.
    pub fn read32(&self, reg: u16) -> io::Result<u32> {
        self.read_reg(reg, ValueWidth::Bits32)
    }

    /// Writes a register of the given width.
    pub fn write_reg(&self, reg: u16, width: ValueWidth, value: u32) -> io::Result<()> {
        let (buf, len) = encode_write(reg, self.addr_width, value, width)?;
        self.transfer(&mut [Message::Write(&buf[..len])])
    }

    /// Writes an 8-bit register.
    pub fn write8(&self, reg: u16, value: u8) -> io::Result<()> {
        self.write_reg(reg, ValueWidth::Bits8, value.into())
    }

    /// Writes a 16-bit register.
    pub fn write16(&self, reg: u16, value: u16) -> io::Result<()> {
        self.write_reg(reg, ValueWidth::Bits16, value.into())
    }

    /// Writes a 32-bit register.
    pub fn write32(&self, reg: u16, value: u32) -> io::Result<()> {
        self.write_reg(reg, ValueWidth::Bits32, value)
    }

    /// Writes `data` to consecutive registers starting at `reg`, in one message.
    pub fn write_burst(&self, reg: u16, data: &[u8]) -> io::Result<()> {
        let buf = encode_burst(reg, self.addr_width, data)?;
        self.transfer(&mut [Message::Write(&buf)])
    }

    /// Writes a list of `(register, value)` pairs of one width, packing up to
    /// [`MAX_MESSAGES`] writes into each combined transfer.
    pub fn write_sequence(&self, width: ValueWidth, writes: &[(u16, u32)]) -> io::Result<()> {
        for chunk in writes.chunks(MAX_MESSAGES) {
            let encoded = chunk
                .iter()
                .map(|&(reg, value)| encode_write(reg, self.addr_width, value, width))
                .collect::<io::Result<Vec<_>>>()?;
            let mut messages: Vec<Message<'_>> = encoded
                .iter()
                .map(|(buf, len)| Message::Write(&buf[..*len]))
                .collect();
            self.transfer(&mut messages)?;
        }
        Ok(())
    }
}

/// Encodes a burst write: the register address followed by `data`.
pub fn encode_burst(reg: u16, addr: AddrWidth, data: &[u8]) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; addr.bytes() + data.len()];
    let n = encode_reg_addr(reg, addr, &mut buf)?;
    buf[n..].copy_from_slice(data);
    msg_len(buf.len())?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_layouts_match_the_kernel() {
        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(size_of::<I2cMsg>(), 16);
            assert_eq!(size_of::<I2cRdwrData>(), 16);
        }
        assert_eq!(std::mem::offset_of!(I2cMsg, len), 4);
    }

    #[test]
    fn encodes_register_writes() {
        let (b, n) = encode_write(0x3008, AddrWidth::Bits16, 0x82, ValueWidth::Bits8).unwrap();
        assert_eq!(&b[..n], &[0x30, 0x08, 0x82]);
        let (b, n) = encode_write(0x380c, AddrWidth::Bits16, 0x05b0, ValueWidth::Bits16).unwrap();
        assert_eq!(&b[..n], &[0x38, 0x0c, 0x05, 0xb0]);
        let (b, n) = encode_write(0x10, AddrWidth::Bits8, 0x1234_5678, ValueWidth::Bits32).unwrap();
        assert_eq!(&b[..n], &[0x10, 0x12, 0x34, 0x56, 0x78]);
        assert!(encode_write(0x100, AddrWidth::Bits8, 0, ValueWidth::Bits8).is_err());
        assert!(encode_write(0x10, AddrWidth::Bits8, 0x100, ValueWidth::Bits8).is_err());
    }

    #[test]
    fn decodes_big_endian_values() {
        assert_eq!(decode_value(&[0x97]), 0x97);
        assert_eq!(decode_value(&[0x97, 0x82]), 0x9782);
        assert_eq!(decode_value(&[1, 2, 3, 4]), 0x0102_0304);
    }

    #[test]
    fn builds_combined_read() {
        let addr = [0x30, 0x0a];
        let mut out = [0u8; 2];
        let mut messages = [Message::Write(&addr), Message::Read(&mut out)];
        let msgs = build_messages(0x60, &mut messages).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!((msgs[0].addr, msgs[0].flags, msgs[0].len), (0x60, 0, 2));
        assert_eq!(
            (msgs[1].addr, msgs[1].flags, msgs[1].len),
            (0x60, I2C_M_RD, 2)
        );
    }

    #[test]
    fn rejects_bad_transfers() {
        assert!(build_messages(0x60, &mut []).is_err());
        let empty: [u8; 0] = [];
        assert!(build_messages(0x60, &mut [Message::Write(&empty)]).is_err());
        let data = [0u8; 1];
        let mut many: Vec<Message<'_>> =
            (0..=MAX_MESSAGES).map(|_| Message::Write(&data)).collect();
        assert!(build_messages(0x60, &mut many).is_err());
        assert!(encode_burst(0, AddrWidth::Bits16, &vec![0u8; MAX_MESSAGE_LEN]).is_err());
    }

    #[test]
    fn encodes_bursts() {
        assert_eq!(
            encode_burst(0x5000, AddrWidth::Bits16, &[1, 2, 3]).unwrap(),
            vec![0x50, 0x00, 1, 2, 3]
        );
    }

    #[test]
    fn open_missing_bus_fails_cleanly() {
        let err =
            I2cDevice::open_path("/dev/i2c-does-not-exist", 0x60, AddrWidth::Bits16).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(I2cDevice::open(0, 0x80, AddrWidth::Bits8).is_err());
    }

    /// Opens every i2c-dev node present, claiming an address nothing uses on a typical system,
    /// without transferring. Skips when there are none.
    #[test]
    fn claims_addresses_on_present_buses() {
        let Ok(dir) = std::fs::read_dir("/dev") else {
            return;
        };
        for entry in dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("i2c-") {
                continue;
            }
            match I2cDevice::open_path(entry.path(), 0x7e, AddrWidth::Bits8) {
                Ok(dev) => assert_eq!(dev.address(), 0x7e),
                Err(e) => eprintln!("skipping {name}: {e}"),
            }
        }
    }
}
