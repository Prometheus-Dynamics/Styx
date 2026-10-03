//! The committed SPIR-V is what `shaders/build.sh` makes from the GLSL (skipped without
//! `glslc`).

use std::path::Path;
use std::process::Command;

#[test]
fn spirv_is_up_to_date() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("shaders");
    let out = std::env::temp_dir().join(format!("styx-gpuisp-spv-{}", std::process::id()));
    std::fs::create_dir_all(&out).unwrap();
    for s in ["full", "half", "stats"] {
        let spv = out.join(format!("{s}.spv"));
        let status = Command::new("glslc")
            .args(["--target-env=vulkan1.2", "-O", "-o"])
            .arg(&spv)
            .arg(dir.join(format!("{s}.comp")))
            .status();
        match status {
            Ok(st) if st.success() => {}
            Ok(st) => panic!("glslc failed on {s}.comp: {st}"),
            Err(_) => {
                eprintln!("no glslc: SPIR-V check skipped");
                return;
            }
        }
        let committed = std::fs::read(dir.join(format!("{s}.spv"))).unwrap();
        assert!(
            std::fs::read(&spv).unwrap() == committed,
            "{s}.spv is stale: run crates/gpuisp/shaders/build.sh"
        );
    }
    let _ = std::fs::remove_dir_all(&out);
}
