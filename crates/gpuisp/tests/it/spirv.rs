//! The committed shader modules against a rule `glslc -O` can break unnoticed: folding a store
//! to an 8-bit buffer makes an 8- or 16-bit constant, valid only with the `Int8` or `Int16`
//! capability (`VUID-VkShaderModuleCreateInfo-pCode-08737`).

const OP_CAPABILITY: u32 = 17;
const OP_TYPE_INT: u32 = 21;
const OP_CONSTANT: u32 = 43;
const CAPABILITY_INT16: u32 = 22;
const CAPABILITY_INT8: u32 = 39;

/// The widths of the constants of `spv` narrower than 32 bits its capabilities do not allow.
fn invalid_constants(spv: &[u8]) -> Vec<u32> {
    let w: Vec<u32> = spv
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    assert_eq!(w[0], 0x0723_0203, "not SPIR-V");
    let (mut caps, mut ints, mut consts) = (Vec::new(), Vec::new(), Vec::new());
    let mut i = 5;
    while i < w.len() {
        let (op, len) = (w[i] & 0xffff, (w[i] >> 16) as usize);
        assert_ne!(len, 0, "malformed module");
        match op {
            OP_CAPABILITY => caps.push(w[i + 1]),
            OP_TYPE_INT => ints.push((w[i + 1], w[i + 2])),
            OP_CONSTANT => consts.push(w[i + 1]),
            _ => {}
        }
        i += len;
    }
    consts
        .iter()
        .filter_map(|t| ints.iter().find(|(id, _)| id == t).map(|&(_, width)| width))
        .filter(|&width| {
            (width == 8 && !caps.contains(&CAPABILITY_INT8))
                || (width == 16 && !caps.contains(&CAPABILITY_INT16))
        })
        .collect()
}

#[test]
fn no_shader_has_a_constant_its_capabilities_do_not_allow() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/shaders");
    let mut seen = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "spv") {
            seen += 1;
            let bad = invalid_constants(&std::fs::read(&path).unwrap());
            assert!(bad.is_empty(), "{}: constants {bad:?}", path.display());
        }
    }
    assert!(seen > 0, "no modules in {dir}");
}

#[test]
fn a_narrow_constant_without_the_capability_is_found() {
    // Header, then OpTypeInt %1 8 0 and OpConstant %1 %2 255.
    let words: [u32; 13] = [
        0x0723_0203,
        0,
        0,
        3,
        0,
        (4 << 16) | OP_TYPE_INT,
        1,
        8,
        0,
        (4 << 16) | OP_CONSTANT,
        1,
        2,
        255,
    ];
    let spv: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    assert_eq!(invalid_constants(&spv), [8]);
}
