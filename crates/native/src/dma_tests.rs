use super::*;

#[test]
fn a_memfd_maps_like_a_dma_buf_and_outlives_its_descriptor() {
    let buf = DmaBuf::memfd("styx-test", 8192).unwrap();
    assert_eq!(buf.len(), 8192);
    let mut a = buf.map().unwrap();
    a.as_mut_slice()[100] = 7;
    let b = DmaBuf::from_fd(buf.into_fd(), 8192).map().unwrap();
    // The descriptor is closed; both mappings still share the memory.
    assert_eq!(b.as_slice()[100], 7);
    assert!(DmaBuf::memfd("empty", 0).is_err());
}

#[test]
fn heap_buffers_have_tail_slack_in_whole_pages() {
    assert_eq!(padded_len(1_228_800), 1_232_896); // 1280x800 GREY: v3dv wants 1_229_056
    assert!(padded_len(1_228_800) >= 1_228_800 + TAIL_SLACK);
    assert_eq!(padded_len(4096 - TAIL_SLACK), 4096);
    assert_eq!(padded_len(4096), 8192);
}

#[test]
fn dmabuf_size_reports_what_the_kernel_allocated() {
    let buf = DmaBuf::memfd("styx-test", 12_345).unwrap();
    assert_eq!(dmabuf_size(buf.as_fd()).unwrap(), 12_345);
    // Twice: the offset went back to the start.
    assert_eq!(dmabuf_size(buf.as_fd()).unwrap(), 12_345);
}

#[test]
fn rejects_bad_names() {
    assert!(DmaHeap::open("../etc").is_err());
    assert!(DmaHeap::open("").is_err());
}

#[test]
fn allocates_and_syncs_when_a_heap_exists() {
    let Some(name) = list_heaps()
        .into_iter()
        .find(|n| n == "system")
        .or_else(|| list_heaps().pop())
    else {
        eprintln!("no dma-heaps; skipping");
        return;
    };
    let heap = match DmaHeap::open(&name) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("cannot open heap {name}: {e}; skipping");
            return;
        }
    };
    let buf = heap.allocate(4096).expect("allocate");
    let mut map = buf.map().expect("map");
    buf.begin_cpu_access(Access::Write).expect("sync start");
    map.as_mut_slice().fill(0xa5);
    buf.end_cpu_access(Access::Write).expect("sync end");
    assert!(map.as_slice().iter().all(|&b| b == 0xa5));
}
