//! What a UVC camera sends while streaming and negotiating: payload headers and frame assembly
//! from a stream of (possibly lost) payloads, PTS/SCR clock recovery, the PROBE/COMMIT
//! structure and control values.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use styx_uvc::clock::{BusClock, DeviceClock};
use styx_uvc::controls::{CONTROLS, ControlDef};
use styx_uvc::payload::{Assembler, Payload, PayloadHeader};
use styx_uvc::pool::BufferPool;
use styx_uvc::probe::StreamingParams;

#[derive(Arbitrary, Debug)]
struct Packet {
    data: Vec<u8>,
    failed: bool,
    /// Packets skipped before this one (lost on the bus).
    gap: u8,
    /// Ends a URB of this many packets (0: not the end of one).
    urb_end: u8,
}

#[derive(Arbitrary, Debug)]
struct Input {
    capacity: u16,
    expected: Option<u16>,
    keep: u8,
    clock_hz: u32,
    high_speed: bool,
    packets: Vec<Packet>,
    reset_at: Option<u8>,
    probe: Vec<u8>,
    probe_version: u16,
    control: Vec<u8>,
    control_value: i64,
    res: u8,
}

fuzz_target!(|input: Input| {
    let interval_ns = if input.high_speed { 125_000 } else { 1_000_000 };
    let mut asm = Assembler::new(
        BufferPool::new(usize::from(input.capacity), usize::from(input.keep % 4)),
        input.expected.map(usize::from),
    );
    let mut bus = BusClock::new(interval_ns);
    let mut clock = DeviceClock::new(input.clock_hz, if input.high_speed { 8 } else { 1 });
    let mut out = Vec::new();
    let mut packet = 0u64;
    let mut reaped = 1_000_000_000u64;
    for (i, p) in input.packets.iter().enumerate() {
        if input.reset_at.is_some_and(|r| usize::from(r) == i) {
            asm.reset();
        }
        packet += u64::from(p.gap);
        if p.urb_end > 0 {
            reaped += u64::from(p.urb_end) * interval_ns + u64::from(p.gap) * 1_000;
            bus.urb(reaped, u64::from(p.urb_end));
        }
        if let Some(h) = PayloadHeader::parse(&p.data) {
            assert!(h.len >= 2 && h.len <= p.data.len());
            if let Some(scr) = h.scr {
                clock.scr(scr, packet, &bus);
            }
            if let Some(pts) = h.pts {
                let _ = clock.to_host(pts);
            }
        }
        asm.push(
            Payload {
                data: &p.data,
                failed: p.failed,
                packet,
                time_ns: bus.packet_time(packet),
            },
            &mut out,
        );
        for frame in out.drain(..) {
            assert!(frame.data.len() <= usize::from(input.capacity).max(frame.data.capacity()));
            if let Some(e) = input.expected {
                assert!(frame.data.len() <= usize::from(e));
            }
            let _ = frame.flags.damaged();
        }
        packet += 1;
    }
    let _ = (asm.stats(), clock.samples(), bus.packet_start(packet));

    let params = StreamingParams::decode(&input.probe);
    let len = StreamingParams::len_for(input.probe_version);
    let encoded = params.encode(len);
    assert_eq!(encoded.len(), len);
    let back = StreamingParams::decode(&encoded);
    assert_eq!(back.frame_interval, params.frame_interval);

    for def in CONTROLS {
        let v = def.decode(&input.control);
        let _ = def.decode(&def.encode(v));
        assert_eq!(def.encode(input.control_value).len(), usize::from(def.size));
    }
    let _ = ControlDef::ae_menu(input.res);
});
