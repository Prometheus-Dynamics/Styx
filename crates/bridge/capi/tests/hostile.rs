//! Every entry point survives NULL and garbage arguments, the header and the library agree on the
//! names, statuses and sizes, and the statuses and enum values are the ones the header states.
//!
//! A garbage argument here is a wrong length, index, enum value, text or NULL where the call needs
//! a pointer. A non-NULL pointer to memory that is not a handle is undefined, and is not tested.

use std::collections::BTreeSet;
use std::path::Path;

use styx_capi::*;

fn read(path: &str) -> String {
    let full = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("{}: {e}", full.display()))
}

/// Calls every entry point with NULL and with garbage. Each call must return a status (no panic,
/// no crash, no unwind).
#[test]
fn every_entry_point_survives_null_and_garbage_arguments() {
    let mut value = 0_u32;
    let mut minor = 0_u32;
    let mut len = 0_usize;
    let mut source_out: *mut styx_capi::Source = std::ptr::null_mut();
    let mut camera_out: *mut styx_capi::Camera = std::ptr::null_mut();
    let mut frame_out: *mut styx_capi::Frame = std::ptr::null_mut();
    let garbage: [u8; 4] = [0xff, 0xfe, 0x00, 0x80];
    let zero_value = StyxControlValue { kind: 0, value: 0 };
    let bogus_value = StyxControlValue { kind: 99, value: 7 };
    let mut out_value = StyxControlValue { kind: 0, value: 0 };
    let mut info = FrameInfo {
        id: 0,
        timestamp_ns: 0,
        fourcc: 0,
        width: 0,
        height: 0,
        planes: 0,
        fps_milli: 0,
        exposure_us: 0,
        analogue_gain_milli: 0,
        memory_kind: 0,
        achieved_fps_milli: 0,
    };
    let mut plane = PlaneOut {
        data: std::ptr::null(),
        len: 0,
        stride: 0,
    };
    let (mut fd, mut offset, mut kind): (i32, u64, u32) = (0, 0, 0);
    let (mut copies, mut bytes) = (0_u64, 0_u64);
    let mut buf = [0_u8; 8];
    let opts = StyxOpenOptions {
        width: 0,
        height: 0,
        fps_milli: 0,
        exposure_us: 0,
        gain_milli: 0,
    };

    // SAFETY: the arguments are NULL, zero or garbage values the calls must refuse by status; no
    // call dereferences a pointer it was not given a valid one for, and every out-pointer points at
    // a local that outlives the call.
    unsafe {
        // Version and status.
        assert_eq!(
            styx_version(std::ptr::null_mut(), std::ptr::null_mut()),
            E_INVALID
        );
        assert_eq!(styx_version(&mut value, std::ptr::null_mut()), E_INVALID);
        assert_eq!(styx_version(&mut value, &mut minor), OK);
        assert_eq!((value, minor), (ABI_MAJOR, ABI_MINOR));
        assert!(styx_status_name(i32::MIN).is_null());
        assert!(!styx_status_name(E_INVALID).is_null());
        assert_eq!(
            styx_last_error(std::ptr::null_mut(), 0, std::ptr::null_mut()),
            E_INVALID
        );
        assert_eq!(
            styx_last_error(buf.as_mut_ptr(), usize::MAX, &mut len),
            E_INVALID
        );
        assert_eq!(
            styx_last_error(std::ptr::null_mut(), 4, &mut len),
            E_INVALID
        );
        assert_eq!(styx_last_error(buf.as_mut_ptr(), buf.len(), &mut len), OK);

        // Sources.
        assert_eq!(
            styx_source_local(std::ptr::null(), 0, std::ptr::null_mut()),
            E_INVALID
        );
        assert_eq!(
            styx_source_local(std::ptr::null(), 5, &mut source_out),
            E_INVALID
        );
        assert_eq!(
            styx_source_local(garbage.as_ptr(), 0, &mut source_out),
            E_INVALID
        );
        assert_eq!(
            styx_source_local(garbage.as_ptr(), usize::MAX, &mut source_out),
            E_INVALID
        );
        assert_eq!(
            styx_source_service(std::ptr::null(), 0, std::ptr::null_mut()),
            E_INVALID
        );
        assert_eq!(
            styx_source_service(garbage.as_ptr(), usize::MAX, &mut source_out),
            E_INVALID
        );
        // A path of non-UTF-8 bytes is a path on Unix: the source is made, and listing it fails
        // with an I/O status (there is no daemon at that path).
        assert_eq!(
            styx_source_local(garbage.as_ptr(), garbage.len(), &mut source_out),
            OK
        );
        assert!(!source_out.is_null());
        assert_eq!(
            styx_source_camera_count(source_out, std::ptr::null_mut()),
            E_INVALID
        );
        assert_eq!(styx_source_camera_count(source_out, &mut len), E_IO);
        assert_eq!(
            styx_source_camera_name(source_out, 0, std::ptr::null_mut(), 0, &mut len),
            E_IO
        );
        assert_eq!(
            styx_source_camera_name(source_out, usize::MAX, buf.as_mut_ptr(), 8, &mut len),
            E_IO
        );
        assert_eq!(
            styx_camera_open(
                source_out,
                std::ptr::null(),
                0,
                std::ptr::null(),
                &mut camera_out
            ),
            E_INVALID,
            "an empty name"
        );
        assert_eq!(
            styx_camera_open(
                source_out,
                garbage.as_ptr(),
                garbage.len(),
                &opts,
                &mut camera_out
            ),
            E_INVALID,
            "a name that is not UTF-8"
        );
        assert_eq!(
            styx_camera_open(
                source_out,
                b"lemnos:cam0-sensor".as_ptr(),
                18,
                std::ptr::null(),
                &mut camera_out
            ),
            E_IO,
            "no daemon at that path"
        );
        assert!(camera_out.is_null());
        styx_source_close(source_out);
        styx_source_close(std::ptr::null_mut());
        assert_eq!(
            styx_source_camera_count(std::ptr::null(), &mut len),
            E_INVALID
        );
        assert_eq!(
            styx_source_camera_name(std::ptr::null(), 0, buf.as_mut_ptr(), 8, &mut len),
            E_INVALID
        );
        assert_eq!(
            styx_camera_open(
                std::ptr::null(),
                b"x".as_ptr(),
                1,
                std::ptr::null(),
                &mut camera_out
            ),
            E_INVALID
        );

        // Cameras.
        assert_eq!(
            styx_camera_open(
                std::ptr::null(),
                std::ptr::null(),
                0,
                std::ptr::null(),
                std::ptr::null_mut()
            ),
            E_INVALID
        );
        styx_camera_close(std::ptr::null_mut());
        assert_eq!(
            styx_camera_set_control(std::ptr::null_mut(), b"exposure".as_ptr(), 8, zero_value, 0),
            E_INVALID
        );
        assert_eq!(
            styx_camera_set_control(
                std::ptr::null_mut(),
                std::ptr::null(),
                3,
                bogus_value,
                u64::MAX
            ),
            E_INVALID
        );
        assert_eq!(
            styx_camera_get_control(std::ptr::null_mut(), b"gain".as_ptr(), 4, &mut out_value),
            E_INVALID
        );
        assert_eq!(
            styx_camera_next_frame(std::ptr::null_mut(), u32::MAX, &mut frame_out),
            E_INVALID
        );
        assert_eq!(
            styx_camera_next_frame(std::ptr::null_mut(), 0, std::ptr::null_mut()),
            E_INVALID
        );

        // Frames.
        assert_eq!(styx_frame_describe(std::ptr::null(), &mut info), E_INVALID);
        assert_eq!(
            styx_frame_describe(std::ptr::null(), std::ptr::null_mut()),
            E_INVALID
        );
        assert_eq!(
            styx_frame_plane(std::ptr::null(), u32::MAX, &mut plane),
            E_INVALID
        );
        assert_eq!(
            styx_frame_metadata(std::ptr::null(), buf.as_mut_ptr(), 8, &mut len),
            E_INVALID
        );
        assert_eq!(
            styx_frame_metadata(std::ptr::null(), std::ptr::null_mut(), usize::MAX, &mut len),
            E_INVALID
        );
        assert_eq!(
            styx_frame_fd(
                std::ptr::null(),
                &mut fd,
                &mut offset,
                std::ptr::null_mut(),
                &mut kind
            ),
            E_INVALID
        );
        styx_frame_release(std::ptr::null_mut());
        assert_eq!(
            styx_copies(std::ptr::null_mut(), std::ptr::null_mut()),
            E_INVALID
        );
        assert_eq!(styx_copies(&mut copies, &mut bytes), OK);
        assert_eq!(copies, 0);

        // The metadata size, and the counters decoder.
        assert_eq!(styx_frame_metadata_size(std::ptr::null_mut()), E_INVALID);
        assert_eq!(styx_frame_metadata_size(&mut len), OK);
        assert_eq!(len, styx_controls::WIRE_BYTES);
        assert_eq!(
            styx_counters_read(std::ptr::null(), 0, std::ptr::null_mut()),
            E_INVALID
        );
        let mut counters = CounterSnapshot::default();
        assert_eq!(
            styx_counters_read(std::ptr::null(), 4, &mut counters),
            E_INVALID
        );
        assert_eq!(
            styx_counters_read(garbage.as_ptr(), garbage.len(), &mut counters),
            E_INVALID
        );
        assert_eq!(
            styx_counters_read(garbage.as_ptr(), garbage.len(), std::ptr::null_mut()),
            E_INVALID
        );
    }
    // Every status has a name; a status that is not one has none.
    assert!(!styx_status_name(E_PANIC).is_null());
    assert!(styx_status_name(-1000).is_null());
}

/// The header's statuses and the library's constants are the same numbers.
#[test]
fn the_header_and_the_library_agree_on_statuses_and_enum_values() {
    let header = read("include/styx.h");
    let table: [(&str, i32); 11] = [
        ("STYX_OK", OK),
        ("STYX_E_INVALID", E_INVALID),
        ("STYX_E_NOT_FOUND", E_NOT_FOUND),
        ("STYX_E_IO", E_IO),
        ("STYX_E_TIMEOUT", E_TIMEOUT),
        ("STYX_E_UNSUPPORTED", E_UNSUPPORTED),
        ("STYX_E_BUSY", E_BUSY),
        ("STYX_E_PROTOCOL", E_PROTOCOL),
        ("STYX_E_PANIC", E_PANIC),
        ("STYX_E_STOPPED", E_STOPPED),
        ("STYX_E_RANGE", E_RANGE),
    ];
    for (name, value) in table {
        let line = header
            .lines()
            .find(|l| l.starts_with(&format!("#define {name} ")))
            .unwrap_or_else(|| panic!("the header defines {name}"));
        let value_text = line[format!("#define {name} ").len()..]
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_start_matches('(')
            .trim_end_matches(')');
        let text = value_text.to_string();
        let number: i32 = text
            .parse()
            .unwrap_or_else(|_| panic!("{name} is a number in the header: {line}"));
        assert_eq!(number, value, "{name}");
    }
    assert!(header.contains(&format!("#define STYX_ABI_MAJOR {ABI_MAJOR}")));
    assert!(header.contains(&format!(
        "#define STYX_METADATA_BYTES {}",
        styx_controls::WIRE_BYTES
    )));
    assert!(header.contains(&format!("#define STYX_ABI_MINOR {ABI_MINOR}")));
    for (name, value) in [
        ("STYX_VALUE_AUTO", VALUE_AUTO),
        ("STYX_VALUE_MANUAL", VALUE_MANUAL),
        ("STYX_VALUE_FLAG", VALUE_FLAG),
    ] {
        assert!(
            header.contains(&format!("#define {name} {value}")),
            "{name}"
        );
    }
    for (name, value) in [
        ("STYX_MEMORY_HOST", MEMORY_HOST),
        ("STYX_MEMORY_MEMFD", MEMORY_MEMFD),
        ("STYX_MEMORY_DMABUF", MEMORY_DMABUF),
        ("STYX_MEMORY_GPU", MEMORY_GPU),
    ] {
        assert!(
            header.contains(&format!("#define {name} {value}")),
            "{name}"
        );
    }
}

/// Every function the header declares is exported by the library, and the library exports no
/// other C function.
#[test]
fn the_header_declares_exactly_the_functions_the_library_exports() {
    // A declaration starts a line at column 0 and names its function before the first `(`.
    let header = strip_comments(&read("include/styx.h"));
    let mut declared = BTreeSet::new();
    for line in header.lines() {
        if line.starts_with(|c: char| c.is_ascii_alphabetic()) && !line.starts_with("typedef") {
            for (at, _) in line.match_indices("styx_") {
                let name: String = line[at..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if line[at + name.len()..].trim_start().starts_with('(') {
                    declared.insert(name);
                    break;
                }
            }
        }
    }
    let source = read("src/surface.rs");
    let mut exported = BTreeSet::new();
    for line in source.lines() {
        let line = line.trim_start();
        if let Some(rest) = line
            .strip_prefix("pub unsafe extern \"C\" fn ")
            .or_else(|| line.strip_prefix("pub extern \"C\" fn "))
        {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            exported.insert(name);
        }
    }
    assert_eq!(
        declared.len(),
        21,
        "the header declares 21 functions: {declared:?}"
    );
    assert_eq!(declared, exported, "the header and the exports agree");
}

fn strip_comments(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        match rest[start..].find("*/") {
            Some(end) => rest = &rest[start + end + 2..],
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}
