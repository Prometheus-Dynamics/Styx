//! Little-endian TIFF writing: IFDs of typed entries, nested IFDs (SubIFDs, the EXIF IFD) and
//! one strip of image data per IFD.
//!
//! Layout: header, then every IFD in tree order (each followed by the values that do not fit
//! in its entries), then the image strips. Offsets are computed before anything is written.

use alloc::collections::BTreeMap;
use alloc::{string::String, vec, vec::Vec};

#[cfg(not(feature = "std"))]
use crate::math::Float as _;

/// TIFF field types.
pub(crate) mod kind {
    pub const BYTE: u16 = 1;
    pub const ASCII: u16 = 2;
    pub const SHORT: u16 = 3;
    pub const LONG: u16 = 4;
    pub const RATIONAL: u16 = 5;
    pub const SBYTE: u16 = 6;
    pub const UNDEFINED: u16 = 7;
    pub const SSHORT: u16 = 8;
    pub const SLONG: u16 = 9;
    pub const SRATIONAL: u16 = 10;
    pub const FLOAT: u16 = 11;
    pub const DOUBLE: u16 = 12;
    pub const IFD: u16 = 13;
}

/// Tags used by the writer and the reader.
#[allow(dead_code)]
pub(crate) mod tag {
    pub const NEW_SUBFILE_TYPE: u16 = 254;
    pub const IMAGE_WIDTH: u16 = 256;
    pub const IMAGE_LENGTH: u16 = 257;
    pub const BITS_PER_SAMPLE: u16 = 258;
    pub const COMPRESSION: u16 = 259;
    pub const PHOTOMETRIC: u16 = 262;
    pub const IMAGE_DESCRIPTION: u16 = 270;
    pub const MAKE: u16 = 271;
    pub const MODEL: u16 = 272;
    pub const STRIP_OFFSETS: u16 = 273;
    pub const ORIENTATION: u16 = 274;
    pub const SAMPLES_PER_PIXEL: u16 = 277;
    pub const ROWS_PER_STRIP: u16 = 278;
    pub const STRIP_BYTE_COUNTS: u16 = 279;
    pub const PLANAR_CONFIGURATION: u16 = 284;
    pub const SOFTWARE: u16 = 305;
    pub const DATE_TIME: u16 = 306;
    pub const TILE_WIDTH: u16 = 322;
    pub const TILE_LENGTH: u16 = 323;
    pub const TILE_OFFSETS: u16 = 324;
    pub const TILE_BYTE_COUNTS: u16 = 325;
    pub const SUB_IFDS: u16 = 330;
    pub const SAMPLE_FORMAT: u16 = 339;
    pub const CFA_REPEAT_PATTERN_DIM: u16 = 33421;
    pub const CFA_PATTERN: u16 = 33422;
    pub const EXPOSURE_TIME: u16 = 33434;
    pub const F_NUMBER: u16 = 33437;
    pub const EXIF_IFD: u16 = 34665;
    pub const ISO_SPEED_RATINGS: u16 = 34855;
    pub const SENSITIVITY_TYPE: u16 = 34864;
    pub const EXIF_VERSION: u16 = 36864;
    pub const DATE_TIME_ORIGINAL: u16 = 36867;
    pub const USER_COMMENT: u16 = 37510;
    pub const SUB_SEC_TIME_ORIGINAL: u16 = 37521;
    pub const DNG_VERSION: u16 = 50706;
    pub const DNG_BACKWARD_VERSION: u16 = 50707;
    pub const UNIQUE_CAMERA_MODEL: u16 = 50708;
    pub const CFA_PLANE_COLOR: u16 = 50710;
    pub const CFA_LAYOUT: u16 = 50711;
    pub const LINEARIZATION_TABLE: u16 = 50712;
    pub const BLACK_LEVEL_REPEAT_DIM: u16 = 50713;
    pub const BLACK_LEVEL: u16 = 50714;
    pub const BLACK_LEVEL_DELTA_H: u16 = 50715;
    pub const BLACK_LEVEL_DELTA_V: u16 = 50716;
    pub const WHITE_LEVEL: u16 = 50717;
    pub const DEFAULT_SCALE: u16 = 50718;
    pub const DEFAULT_CROP_ORIGIN: u16 = 50719;
    pub const DEFAULT_CROP_SIZE: u16 = 50720;
    pub const COLOR_MATRIX_1: u16 = 50721;
    pub const COLOR_MATRIX_2: u16 = 50722;
    pub const CAMERA_CALIBRATION_1: u16 = 50723;
    pub const CAMERA_CALIBRATION_2: u16 = 50724;
    pub const ANALOG_BALANCE: u16 = 50727;
    pub const AS_SHOT_NEUTRAL: u16 = 50728;
    pub const AS_SHOT_WHITE_XY: u16 = 50729;
    pub const BASELINE_EXPOSURE: u16 = 50730;
    pub const CALIBRATION_ILLUMINANT_1: u16 = 50778;
    pub const CALIBRATION_ILLUMINANT_2: u16 = 50779;
    pub const ACTIVE_AREA: u16 = 50829;
    pub const FORWARD_MATRIX_1: u16 = 50964;
    pub const FORWARD_MATRIX_2: u16 = 50965;
    pub const PREVIEW_COLOR_SPACE: u16 = 50970;
    pub const OPCODE_LIST_1: u16 = 51008;
    pub const OPCODE_LIST_2: u16 = 51009;
    pub const OPCODE_LIST_3: u16 = 51022;
}

/// An entry's value.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Value {
    Byte(Vec<u8>),
    Ascii(String),
    Short(Vec<u16>),
    Long(Vec<u32>),
    Rational(Vec<(u32, u32)>),
    SRational(Vec<(i32, i32)>),
    Undefined(Vec<u8>),
}

impl Value {
    fn kind(&self) -> u16 {
        match self {
            Self::Byte(_) => kind::BYTE,
            Self::Ascii(_) => kind::ASCII,
            Self::Short(_) => kind::SHORT,
            Self::Long(_) => kind::LONG,
            Self::Rational(_) => kind::RATIONAL,
            Self::SRational(_) => kind::SRATIONAL,
            Self::Undefined(_) => kind::UNDEFINED,
        }
    }

    fn count(&self) -> u32 {
        (match self {
            Self::Byte(v) | Self::Undefined(v) => v.len(),
            // NUL-terminated.
            Self::Ascii(s) => s.len() + 1,
            Self::Short(v) => v.len(),
            Self::Long(v) => v.len(),
            Self::Rational(v) => v.len(),
            Self::SRational(v) => v.len(),
        }) as u32
    }

    fn bytes(&self) -> Vec<u8> {
        match self {
            Self::Byte(v) | Self::Undefined(v) => v.clone(),
            Self::Ascii(s) => {
                let mut b = s.as_bytes().to_vec();
                b.push(0);
                b
            }
            Self::Short(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
            Self::Long(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
            Self::Rational(v) => v
                .iter()
                .flat_map(|(n, d)| [n.to_le_bytes(), d.to_le_bytes()].concat())
                .collect(),
            Self::SRational(v) => v
                .iter()
                .flat_map(|(n, d)| [n.to_le_bytes(), d.to_le_bytes()].concat())
                .collect(),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Byte(v) | Self::Undefined(v) => v.len(),
            Self::Ascii(s) => s.len() + 1,
            Self::Short(v) => v.len() * 2,
            Self::Long(v) => v.len() * 4,
            Self::Rational(v) => v.len() * 8,
            Self::SRational(v) => v.len() * 8,
        }
    }
}

/// `v` as a signed rational with denominator `den` (for matrices and exposure offsets).
pub(crate) fn srational(v: f64, den: i32) -> (i32, i32) {
    let n = (v * f64::from(den)).round();
    (
        n.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32,
        den,
    )
}

/// `v` (non-negative) as an unsigned rational with denominator `den`.
pub(crate) fn rational(v: f64, den: u32) -> (u32, u32) {
    let n = (v.max(0.0) * f64::from(den)).round();
    (n.min(f64::from(u32::MAX)) as u32, den)
}

/// An IFD to write.
#[derive(Debug, Default)]
pub(crate) struct Ifd {
    pub entries: BTreeMap<u16, Value>,
    /// Image data, written as one strip (`StripOffsets` / `StripByteCounts` are added).
    pub strip: Option<Vec<u8>>,
    /// IFDs this one points to, by the pointer's tag (`SubIFDs`, `ExifIFD`).
    pub children: Vec<(u16, Vec<Ifd>)>,
}

impl Ifd {
    pub fn set(&mut self, tag: u16, value: Value) {
        self.entries.insert(tag, value);
    }
}

/// An IFD flattened out of the tree, with its children's indices.
struct Flat {
    entries: BTreeMap<u16, Value>,
    strip: Option<Vec<u8>>,
    children: Vec<(u16, Vec<usize>)>,
}

fn flatten(ifd: Ifd, out: &mut Vec<Flat>) -> usize {
    let index = out.len();
    out.push(Flat {
        entries: ifd.entries,
        strip: ifd.strip,
        children: Vec::new(),
    });
    let mut children = Vec::new();
    for (tag, list) in ifd.children {
        let ids = list.into_iter().map(|c| flatten(c, out)).collect();
        children.push((tag, ids));
    }
    out[index].children = children;
    index
}

fn align(v: usize, to: usize) -> usize {
    v.div_ceil(to) * to
}

/// Where everything goes.
struct Layout {
    ifd: Vec<usize>,
    strips: Vec<Option<usize>>,
    total: usize,
}

fn layout(flat: &[Flat]) -> Layout {
    let mut pos = 8;
    let mut ifd = Vec::with_capacity(flat.len());
    for f in flat {
        ifd.push(pos);
        pos += 2 + 12 * f.entries.len() + 4;
        for v in f.entries.values() {
            if v.len() > 4 {
                pos = align(pos + v.len(), 4);
            }
        }
        pos = align(pos, 4);
    }
    let mut strips = Vec::with_capacity(flat.len());
    for f in flat {
        strips.push(f.strip.as_ref().map(|s| {
            let at = align(pos, 4);
            pos = at + s.len();
            at
        }));
    }
    Layout {
        ifd,
        strips,
        total: pos,
    }
}

fn offset(v: usize) -> crate::Result<u32> {
    u32::try_from(v).map_err(|_| crate::invalid("file larger than 4 GiB"))
}

/// Serialises `root` (and everything below it) as a little-endian TIFF file.
pub(crate) fn write(root: Ifd) -> crate::Result<Vec<u8>> {
    let mut flat = Vec::new();
    flatten(root, &mut flat);
    // Placeholders first, so the layout has every entry.
    for f in &mut flat {
        if let Some(s) = &f.strip {
            let len = offset(s.len())?;
            f.entries.insert(tag::STRIP_OFFSETS, Value::Long(vec![0]));
            f.entries
                .insert(tag::STRIP_BYTE_COUNTS, Value::Long(vec![len]));
        }
        for (t, ids) in &f.children {
            f.entries.insert(*t, Value::Long(vec![0; ids.len()]));
        }
    }
    let l = layout(&flat);
    for (f, strip) in flat.iter_mut().zip(&l.strips) {
        if let Some(at) = strip {
            f.entries
                .insert(tag::STRIP_OFFSETS, Value::Long(vec![offset(*at)?]));
        }
        for (t, ids) in &f.children {
            let offsets = ids
                .iter()
                .map(|&c| offset(l.ifd[c]))
                .collect::<crate::Result<Vec<u32>>>()?;
            f.entries.insert(*t, Value::Long(offsets));
        }
    }
    let mut out = vec![0u8; l.total];
    out[..8].copy_from_slice(&[b'I', b'I', 42, 0, 8, 0, 0, 0]);
    for (i, f) in flat.iter().enumerate() {
        let mut pos = l.ifd[i];
        let mut values = pos + 2 + 12 * f.entries.len() + 4;
        out[pos..pos + 2].copy_from_slice(&(f.entries.len() as u16).to_le_bytes());
        pos += 2;
        for (t, v) in &f.entries {
            let e = &mut out[pos..pos + 12];
            e[..2].copy_from_slice(&t.to_le_bytes());
            e[2..4].copy_from_slice(&v.kind().to_le_bytes());
            e[4..8].copy_from_slice(&v.count().to_le_bytes());
            let bytes = v.bytes();
            if bytes.len() <= 4 {
                e[8..8 + bytes.len()].copy_from_slice(&bytes);
            } else {
                e[8..12].copy_from_slice(&offset(values)?.to_le_bytes());
                out[values..values + bytes.len()].copy_from_slice(&bytes);
                values = align(values + bytes.len(), 4);
            }
            pos += 12;
        }
        // Next IFD: none (IFD 0 is the only top-level IFD).
        out[pos..pos + 4].copy_from_slice(&[0; 4]);
        if let (Some(at), Some(s)) = (l.strips[i], &f.strip) {
            out[at..at + s.len()].copy_from_slice(s);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_entries_in_tag_order_with_children_and_strips() {
        let mut exif = Ifd::default();
        exif.set(tag::EXPOSURE_TIME, Value::Rational(vec![(1, 100)]));
        let mut raw = Ifd::default();
        raw.set(tag::IMAGE_WIDTH, Value::Long(vec![2]));
        raw.strip = Some(vec![1, 2, 3, 4, 5]);
        let mut root = Ifd::default();
        root.set(tag::MAKE, Value::Ascii("Styx".into()));
        root.set(tag::IMAGE_WIDTH, Value::Short(vec![4]));
        root.children = vec![(tag::SUB_IFDS, vec![raw]), (tag::EXIF_IFD, vec![exif])];
        let bytes = write(root).unwrap();
        let t = crate::tiff_read::Tiff::parse(&bytes).unwrap();
        let ifd0 = t.ifd(8).unwrap();
        assert_eq!(ifd0.ascii(tag::MAKE).as_deref(), Some("Styx"));
        let tags: Vec<u16> = ifd0.entries.iter().map(|e| e.tag).collect();
        let mut sorted = tags.clone();
        sorted.sort_unstable();
        assert_eq!(tags, sorted);
        let sub = ifd0.u32s(tag::SUB_IFDS).unwrap();
        let raw = t.ifd(sub[0] as usize).unwrap();
        let off = raw.u32s(tag::STRIP_OFFSETS).unwrap()[0] as usize;
        assert_eq!(&bytes[off..off + 5], &[1, 2, 3, 4, 5]);
        let exif = t
            .ifd(ifd0.u32s(tag::EXIF_IFD).unwrap()[0] as usize)
            .unwrap();
        assert_eq!(exif.f64s(tag::EXPOSURE_TIME).unwrap(), vec![0.01]);
    }

    #[test]
    fn rationals_round() {
        assert_eq!(srational(-0.25, 10000), (-2500, 10000));
        assert_eq!(rational(1.5, 2), (3, 2));
        assert_eq!(rational(-1.0, 2), (0, 2));
    }
}
