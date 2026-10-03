use super::*;

/// A payload: header with FID/EOF/ERR and optional PTS/SCR, then `body`.
fn payload(fid: bool, eof: bool, pts: Option<u32>, scr: Option<Scr>, body: &[u8]) -> Vec<u8> {
    let mut flags = 0x80; // EOH
    if fid {
        flags |= FID;
    }
    if eof {
        flags |= EOF;
    }
    let mut h = vec![0, flags];
    if let Some(p) = pts {
        h[1] |= PTS;
        h.extend_from_slice(&p.to_le_bytes());
    }
    if let Some(s) = scr {
        h[1] |= SCR;
        h.extend_from_slice(&s.stc.to_le_bytes());
        h.extend_from_slice(&s.sof.to_le_bytes());
    }
    h[0] = h.len() as u8;
    h.extend_from_slice(body);
    h
}

struct Feed {
    asm: Assembler,
    out: Vec<AssembledFrame>,
    packet: u64,
}

impl Feed {
    fn new(expected: Option<usize>) -> Feed {
        Feed {
            asm: Assembler::new(BufferPool::new(64, 4), expected),
            out: Vec::new(),
            packet: 0,
        }
    }

    fn push(&mut self, data: &[u8]) {
        self.push_with(data, false);
    }

    fn push_with(&mut self, data: &[u8], failed: bool) {
        self.asm.push(
            Payload {
                data,
                failed,
                packet: self.packet,
                time_ns: 1000 * self.packet,
            },
            &mut self.out,
        );
        self.packet += 1;
    }

    fn frames(&mut self) -> Vec<(Vec<u8>, FrameFlags)> {
        self.out
            .drain(..)
            .map(|f| (f.data.to_vec(), f.flags))
            .collect()
    }
}

#[test]
fn parses_headers() {
    let scr = Scr {
        stc: 0x1234_5678,
        sof: 0x7ff,
    };
    let p = payload(true, true, Some(42), Some(scr), b"xy");
    let h = PayloadHeader::parse(&p).unwrap();
    assert_eq!(h.len, 12);
    assert!(h.fid && h.eof && !h.error && !h.still);
    assert_eq!((h.pts, h.scr), (Some(42), Some(scr)));
    // The SOF counter is 11 bits: upper bits are reserved.
    let mut q = p.clone();
    q[11] |= 0xf8;
    assert_eq!(PayloadHeader::parse(&q).unwrap().scr.unwrap().sof, 0x7ff);
    // A length beyond the payload, below 2, or too short for its PTS: invalid.
    assert!(PayloadHeader::parse(&[13, 0x80]).is_none());
    assert!(PayloadHeader::parse(&[1, 0x80]).is_none());
    assert!(PayloadHeader::parse(&[4, 0x84, 0, 0]).is_none());
    assert!(PayloadHeader::parse(&[]).is_none());
    let e = PayloadHeader::parse(&[2, 0x80 | ERR | STI]).unwrap();
    assert!(e.error && e.still);
}

#[test]
fn assembles_frames_ended_by_eof() {
    let mut f = Feed::new(None);
    // Joined mid-frame: dropped up to the first end of frame.
    f.push(&payload(false, false, None, None, b"zz"));
    f.push(&payload(false, true, None, None, b"zz"));
    assert!(f.frames().is_empty());
    // Header-only payloads between frames carry nothing.
    f.push(&payload(false, false, None, None, b""));
    f.push(&payload(true, false, Some(7), None, b"ab"));
    f.push(&payload(true, false, Some(7), None, b"cd"));
    f.push(&payload(true, true, Some(7), None, b"e"));
    f.push(&payload(false, false, None, None, b"fg"));
    f.push(&payload(false, true, None, None, b"h"));
    let frames = f.frames();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].0, b"abcde");
    assert_eq!(frames[1].0, b"fgh");
    assert!(frames.iter().all(|(_, fl)| !fl.damaged() && !fl.no_eof));
    assert_eq!(f.asm.stats().frames, 2);
}

#[test]
fn a_fid_toggle_ends_a_frame_without_eof() {
    let mut f = Feed::new(None);
    // Joined at a FID toggle (no EOF bits at all, as some cameras do).
    f.push(&payload(false, false, None, None, b"partial"));
    f.push(&payload(true, false, None, None, b"ab"));
    f.push(&payload(true, false, None, None, b"c"));
    f.push(&payload(false, false, None, None, b"de"));
    f.push(&payload(true, false, None, None, b"f"));
    let frames = f.frames();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].0, b"abc");
    assert_eq!(frames[1].0, b"de");
    assert!(frames[0].1.no_eof);
    assert!(!frames[0].1.damaged());
}

#[test]
fn payloads_after_eof_without_a_toggle_are_out_of_sync() {
    let mut f = Feed::new(None);
    f.push(&payload(false, true, None, None, b""));
    f.push(&payload(true, true, None, None, b"ab"));
    // Same FID again after EOF: dropped, not a new frame.
    f.push(&payload(true, false, None, None, b"zz"));
    f.push(&payload(false, true, None, None, b"cd"));
    let frames = f.frames();
    assert_eq!(
        frames.iter().map(|x| x.0.clone()).collect::<Vec<_>>(),
        vec![b"ab".to_vec(), b"cd".to_vec()]
    );
    assert_eq!(f.asm.stats().dropped_payloads, 1);
}

#[test]
fn errors_damage_the_frame() {
    let mut f = Feed::new(None);
    f.push(&payload(false, true, None, None, b""));
    // A failed packet in the middle of a frame.
    f.push(&payload(true, false, None, None, b"ab"));
    f.push_with(b"", true);
    f.push(&payload(true, true, None, None, b"cd"));
    // The ERR bit.
    let mut p = payload(false, true, None, None, b"ef");
    p[1] |= ERR;
    f.push(&p);
    // A bad header.
    f.push(&payload(true, false, None, None, b"gh"));
    f.push(&[40, 0x80, 1, 2]);
    f.push(&payload(true, true, None, None, b"ij"));
    // A clean one.
    f.push(&payload(false, true, None, None, b"kl"));
    let frames = f.frames();
    assert_eq!(frames.len(), 4);
    assert!(frames[0].1.error && frames[1].1.error && frames[2].1.error);
    assert!(!frames[3].1.damaged());
    let s = f.asm.stats();
    assert_eq!((s.damaged, s.failed_packets, s.invalid_headers), (3, 1, 1));
}

#[test]
fn a_packet_lost_between_frames_damages_the_next() {
    let mut f = Feed::new(None);
    f.push(&payload(false, true, None, None, b""));
    f.push(&payload(true, true, None, None, b"ab"));
    // Lost: it may have held the next frame's start.
    f.push_with(b"", true);
    f.push(&payload(false, true, None, None, b"cd"));
    let frames = f.frames();
    assert!(!frames[0].1.damaged());
    assert!(frames[1].1.error);
}

#[test]
fn uncompressed_frames_check_their_size() {
    let mut f = Feed::new(Some(4));
    f.push(&payload(false, true, None, None, b""));
    // Short: 3 of 4 bytes.
    f.push(&payload(true, true, None, None, b"abc"));
    // Exact.
    f.push(&payload(false, true, None, None, b"abcd"));
    // Too long: truncated and flagged.
    f.push(&payload(true, false, None, None, b"abc"));
    f.push(&payload(true, true, None, None, b"def"));
    let frames = f.frames();
    assert!(frames[0].1.short);
    assert!(!frames[1].1.damaged());
    assert_eq!(frames[2].0, b"abcd");
    assert!(frames[2].1.overflow);
}

#[test]
fn frames_carry_pts_scr_and_packet_times() {
    let mut f = Feed::new(None);
    f.push(&payload(false, true, None, None, b""));
    let s1 = Scr { stc: 100, sof: 1 };
    let s2 = Scr { stc: 200, sof: 2 };
    f.push(&payload(true, false, Some(55), Some(s1), b"a"));
    f.push(&payload(true, false, Some(55), Some(s2), b"b"));
    f.push(&payload(true, true, Some(55), None, b"c"));
    let fr = &f.out[0];
    assert_eq!(fr.pts, Some(55));
    assert_eq!((fr.scr_first, fr.scr_last), (Some(s1), Some(s2)));
    assert_eq!((fr.first_packet, fr.last_packet), (1, 3));
    assert_eq!((fr.first_time_ns, fr.last_time_ns), (1000, 3000));
    assert_eq!(fr.payloads, 3);
}

#[test]
fn buffers_are_reused() {
    let pool = BufferPool::new(64, 2);
    let mut asm = Assembler::new(pool.clone(), None);
    let mut out = Vec::new();
    let mut fid = false;
    for i in 0..10u64 {
        let p = payload(fid, true, None, None, b"abc");
        asm.push(
            Payload {
                data: &p,
                failed: false,
                packet: i,
                time_ns: i,
            },
            &mut out,
        );
        out.clear();
        fid = !fid;
    }
    assert_eq!(asm.stats().frames, 9);
    assert!(pool.allocations() <= 2, "{}", pool.allocations());
}

#[test]
fn reset_rejoins_at_the_next_boundary() {
    let mut f = Feed::new(None);
    f.push(&payload(false, true, None, None, b""));
    f.push(&payload(true, false, None, None, b"ab"));
    f.asm.reset();
    f.push(&payload(true, true, None, None, b"cd"));
    f.push(&payload(false, true, None, None, b"ef"));
    let frames = f.frames();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].0, b"ef");
}
