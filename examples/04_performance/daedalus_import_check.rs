//! On a device: a software-ISP NV12 frame from a native camera imported into Daedalus's GPU
//! device without a copy (`GpuStorage::import_dmabuf` on the frame's dma-buf). The GPU's read
//! of the import must equal the frame's bytes, and a byte the CPU then writes into the dma-buf
//! must show up in a second GPU read: the storage is the frame's memory, not a copy of it.
//!
//! ```text
//! daedalus_import_check
//! ```
//!
//! The software ISP is asked for (`Accelerators::isp` off) so frames come from its dma-heap
//! output buffers; the import is the storage-buffer path (v3dv cannot sample `LINEAR` images).
//! Raze, 2026-11-02: 1280x800 NV12, 0 of 1536000 bytes differ, the sentinel read back, the
//! import 95-97 µs (dma-buf 1544192 bytes, 1536256 needed).

use std::os::fd::AsRawFd;
use std::time::Duration;

use daedalus::gpu::{GpuBackendKind, GpuOptions, GpuStorage, select_backend};
use styx::imports::framelease::FrameBackingExport;
use styx::planner::{Accelerators, Hardware};
use styx::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let gpu = select_backend(&GpuOptions {
        preferred_backend: Some(GpuBackendKind::Wgpu),
        ..Default::default()
    })?;
    let device = styx::probe_all_with_errors_with_config(&StyxConfig::default())
        .devices
        .into_iter()
        .find(|d| d.backends.iter().any(|b| b.kind == BackendKind::Native))
        .ok_or("no native camera")?;
    let request = Frames::nv12()
        .size_at_most(1280, 800)
        .accelerators(Accelerators {
            isp: Hardware::Off,
            ..Default::default()
        });
    let mut out = styx::planner::plan_many(&device, &[request])?.start()?;
    let mut frames = out.remove(0);
    // Past the start-up frames (AE converging).
    let mut frame = None;
    for _ in 0..10 {
        if let RecvOutcome::Data(f) = frames.next_frame(Duration::from_secs(3)) {
            frame = Some(f);
        }
    }
    let frame = frame.ok_or("no frames")?;
    let res = frame.meta().format.resolution;
    let (w, h) = (res.width.get() as usize, res.height.get() as usize);
    println!(
        "frame: {} {w}x{h}, {:?} ({})",
        frame.meta().format.code,
        frame.residency(),
        frame.external_backing_kind().unwrap_or("owned")
    );
    let FrameBackingExport::DmabufPlanes { planes } = frame.export_backing()? else {
        return Err("the frame is not a dma-buf (no software-ISP dma-heap output)".into());
    };
    let (y, uv) = (&planes[0], &planes[1]);
    println!(
        "planes: Y offset {} len {}, UV offset {} len {}",
        y.offset, y.len, uv.offset, uv.len
    );
    let want: Vec<u8> = frame
        .planes()
        .iter()
        .flat_map(|p| p.data().iter().copied())
        .collect();
    // The compare below covers the whole frame only if the planes are packed back to back.
    if y.offset != 0 || uv.offset != y.len || want.len() != uv.offset + uv.len {
        return Err(format!(
            "planes not packed: Y {}+{}, UV {}+{}, frame data {} bytes",
            y.offset,
            y.len,
            uv.offset,
            uv.len,
            want.len()
        )
        .into());
    }
    let len = (want.len() as u64).next_multiple_of(4);
    let need = GpuStorage::dmabuf_import_len(&gpu, len)?;
    let fd = y.fd.try_clone()?;
    // The frame's own descriptor: `import_dmabuf` takes `fd` and closes it.
    let raw = y.fd.as_raw_fd();
    // SAFETY: lseek on a dma-buf fd reports its size; the offset is not used after.
    let size = unsafe { libc::lseek(raw, 0, libc::SEEK_END) };
    println!("import: {len} bytes, the device needs {need}, the dma-buf has {size}");
    if size < 0 || (size as u64) < need {
        return Err(format!(
            "the dma-buf ({size} bytes) is smaller than the import needs ({need})"
        )
        .into());
    }
    let t = std::time::Instant::now();
    let storage = GpuStorage::import_dmabuf(
        &gpu,
        fd,
        0,
        len,
        None,
        Duration::from_secs(1),
        Some("styx-frame"),
    )?;
    println!("import_dmabuf: {:.1} µs", t.elapsed().as_secs_f64() * 1e6);
    let mut got = vec![0u8; len as usize];
    storage.read_into(0, &mut got)?;
    let differ = want
        .iter()
        .zip(&got[..want.len()])
        .filter(|(a, b)| a != b)
        .count();
    println!(
        "GPU read: {differ} of {} bytes differ (Y {}, UV {})",
        want.len(),
        y.len,
        uv.len
    );
    // A sentinel written through a CPU mapping of the same dma-buf, read back by the GPU. It
    // overwrites the frame's first 4 Y bytes in a pool buffer the ISP reuses: nothing reads
    // this frame after, and it is dropped (back to the pool) below.
    let sentinel = [0x5a, 0xa5, 0x3c, 0xc3];
    // SAFETY: a shared writable mapping of the first page of the frame's dma-buf, unmapped below.
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            raw,
            0,
        )
    };
    if map == libc::MAP_FAILED {
        return Err(std::io::Error::last_os_error().into());
    }
    let synced = sync(raw, true).and_then(|()| {
        // SAFETY: the mapping is at least 4096 bytes.
        unsafe { std::ptr::copy_nonoverlapping(sentinel.as_ptr(), map.cast::<u8>(), 4) };
        sync(raw, false)
    });
    // SAFETY: mapped above.
    unsafe { libc::munmap(map, 4096) };
    synced?;
    let mut head = [0u8; 4];
    storage.read_into(0, &mut head)?;
    let same_memory = head == sentinel;
    println!(
        "sentinel {:02x?} written by the CPU, GPU reads {head:02x?}: {}",
        sentinel,
        if same_memory {
            "same memory, no copy"
        } else {
            "a copy"
        }
    );
    drop(storage);
    drop(frame);
    frames.stop();
    if differ != 0 || !same_memory {
        return Err("import check failed".into());
    }
    println!("pass");
    Ok(())
}

/// `DMA_BUF_IOCTL_SYNC` around CPU writes: `start` before, then the end.
fn sync(fd: i32, start: bool) -> std::io::Result<()> {
    const SYNC_WRITE: u64 = 2;
    const SYNC_END: u64 = 4;
    // _IOW('b', 0, struct dma_buf_sync { __u64 flags; })
    const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x4008_6200;
    let flags: u64 = SYNC_WRITE | if start { 0 } else { SYNC_END };
    // SAFETY: the ioctl reads one u64 from `flags`.
    if unsafe { libc::ioctl(fd, DMA_BUF_IOCTL_SYNC as _, &flags) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
