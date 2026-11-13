//! Grouping on recorded-style timestamp sequences: jitter, a camera that loses frames, and two
//! cameras at different rates. Every case checks the same invariant: each frame received ends up
//! grouped, dropped for one reason, or still pending. Nothing is lost or counted twice.

use styx_group::{DropReason, GroupConfig, GroupPolicy, Grouper, RateMatch};

const MS: u64 = 1_000_000;
const P: u64 = 33_333_333;
const LATENCY: u64 = 5 * MS;

// Recorded-style timestamp sequences (CLOCK_MONOTONIC ns, as a camera would stamp them).
// Generated from a 30 fps / 60 fps model with +-0.3 to +-0.5 ms jitter and frames the sensor
// never delivered (see each test); the values are data, not a capture from hardware.

const CAM30_0: &[u64] = &[
    467773, 33514916, 66318566, 99642211, 133522803, 167135122, 200227066, 233656698, 266624208,
    300407160, 332882215, 366557608, 399510178, 433157811, 466313041, 500279211, 533317224,
    566556445, 600009769, 632986916, 666761029, 700132035, 733219548, 766510160, 800187913,
    833302221, 866435160, 900033159, 933275395, 966575947, 1000078373, 1033061709, 1066993346,
    1099727428, 1133813106, 1166368401, 1200263698, 1233375517, 1266248289, 1299562746,
];

const CAM30_1: &[u64] = &[
    3211126, 36572266, 69456011, 103237251, 136438780, 236301664, 269971036, 302542334, 336116118,
    369448517, 402560819, 435937520, 469428492, 502966705, 536284896, 569279542, 602668392,
    635858910, 702888931, 736718283, 769277922, 802825000, 836181158, 870109787, 902648085,
    935973527, 969168149, 1002773989, 1036671560, 1069940945, 1102815443, 1135952424, 1169569315,
    1202517430, 1236788196, 1269757169, 1302582251,
];

const CAM30_2: &[u64] = &[
    9088419, 41998754, 75476669, 109123629, 142559000, 175663979, 208631352, 242488876, 276002923,
    309425642, 342184622, 375902529, 408532411, 441892969, 475263289, 508622935, 541935502,
    576046591, 608564601, 642434687, 675503906, 708636616, 742321115, 775641626, 809162329,
    842750154, 875448094, 908725353, 942291284, 975205335, 1008658771, 1042671607, 1075319072,
    1142408996, 1175210155, 1208793150, 1242067237, 1275902228, 1308605113,
];

const RATE_0: &[u64] = &[
    21273, 16466007, 33047977, 49803150, 66509396, 83365128, 100050151, 116465506, 133631938,
    150067906, 166582914, 183148035, 200112404, 216633100, 233536045, 250182018, 266371419,
    283524874, 299890436, 316554343, 333391938, 349807112, 366838435, 383152000, 400257181,
    416670325, 433184252, 449852271, 466946661, 483128156, 500088124, 516914883, 533376303,
    550089516, 566620582, 583461369, 600131733, 616428997, 633080006, 649859261, 666828944,
    683174060, 699989670, 716928758, 733147960, 749854215, 766424917, 783303405, 800051564,
    816919704, 833511431, 850224821, 866500912, 883198549, 900089316, 916809705, 933452730,
    950278607, 966790068, 983576315, 999895124, 1016843771, 1033394944, 1049876265, 1066471920,
    1083632021, 1100263080, 1116382010, 1133402528, 1150110755, 1166549320, 1183354354, 1199988218,
    1216532359, 1233188698, 1250250802, 1266388475, 1283373185, 1299993858, 1316568410,
];

const RATE_1: &[u64] = &[
    1810735, 35406631, 68411503, 102289393, 135337576, 168387643, 201869288, 235516708, 268639804,
    301716322, 335093490, 368506415, 402006781, 435342819, 468578419, 501983112, 535623833,
    568905872, 601732443, 635178945, 668495228, 702258264, 735491339, 768883379, 801774931,
    835502508, 868619429, 902223410, 935082042, 968569967, 1001780634, 1035135361, 1068516531,
    1101758787, 1135469275, 1168432179, 1201885091, 1235501972, 1268520773, 1302052697,
];

/// Feeds `cams` in timestamp order, each frame arriving `LATENCY` after its timestamp, and
/// polls after every push. Returns each group taken as (sequence, complete, spread, members).
fn feed(g: &mut Grouper<u64, 3>, cams: &[&[u64]]) -> Vec<(u64, bool, u64, usize)> {
    let mut order: Vec<(u64, usize)> = Vec::new();
    for (cam, frames) in cams.iter().enumerate() {
        order.extend(frames.iter().map(|&ts| (ts, cam)));
    }
    order.sort_unstable();
    let mut out = Vec::new();
    for (ts, cam) in order {
        g.push(cam, ts, ts + LATENCY, ts);
        g.poll(ts + LATENCY);
        while let Some(group) = g.pop() {
            out.push((group.sequence, group.complete, group.spread_ns, group.len()));
        }
    }
    // Let the last deadlines pass.
    g.poll(u64::MAX / 2);
    while let Some(group) = g.pop() {
        out.push((group.sequence, group.complete, group.spread_ns, group.len()));
    }
    out
}

/// `feed` for two cameras: returns the number of groups taken.
fn feed2(g: &mut Grouper<u64, 2>, cams: &[&[u64]]) -> usize {
    let mut order: Vec<(u64, usize)> = Vec::new();
    for (cam, frames) in cams.iter().enumerate() {
        order.extend(frames.iter().map(|&ts| (ts, cam)));
    }
    order.sort_unstable();
    let mut groups = 0;
    for (ts, cam) in order {
        g.push(cam, ts, ts + LATENCY, ts);
        g.poll(ts + LATENCY);
        while g.pop().is_some() {
            groups += 1;
        }
    }
    g.poll(u64::MAX / 2);
    while g.pop().is_some() {
        groups += 1;
    }
    groups
}

/// received = grouped + dropped + pending, per camera and in total.
fn assert_conserved<const N: usize>(g: &Grouper<u64, N>, cameras: usize) {
    let stats = g.stats();
    let mut total_received = 0;
    for cam in 0..cameras {
        let dropped: u64 = DropReason::ALL
            .iter()
            .map(|&r| stats.dropped_by(cam, r))
            .sum();
        let pending = g.pending(cam) as u64;
        assert_eq!(
            stats.received_by(cam),
            stats.grouped_by(cam) + dropped + pending,
            "camera {cam} does not add up"
        );
        total_received += stats.received_by(cam);
    }
    assert_eq!(stats.frames_received(), total_received);
}

fn three_cameras(config: GroupConfig) -> Grouper<u64, 3> {
    let mut g = Grouper::new(config);
    for _ in 0..3 {
        g.add_camera().unwrap();
    }
    g
}

#[test]
fn strict_three_cameras_with_jitter_a_gap_and_a_lost_frame() {
    // 30 fps, tolerance half a frame. Camera 0 delivers 40 frames, camera 1 misses frames 5, 6
    // and 20, camera 2 misses frame 33.
    let mut g = three_cameras(GroupConfig::new(P / 2).deadline_ns(50 * MS));
    let groups = feed(&mut g, &[CAM30_0, CAM30_1, CAM30_2]);
    let stats = g.stats();
    assert_eq!(groups.len(), 36);
    assert!(
        groups.iter().all(|g| g.1 && g.3 == 3),
        "every group is complete"
    );
    for (i, group) in groups.iter().enumerate() {
        assert_eq!(group.0, i as u64);
        assert!(group.2 <= 10 * MS, "spread {} ns", group.2);
    }
    // 116 frames in. The 36 groups hold 108 (one per camera-0 frame); 8 are dropped, each with
    // its reason: a frame of camera 0 whose partner was lost waits out the deadline (Incomplete)
    // or is passed by the partner's next frame (Unmatched).
    assert_eq!(stats.frames_received(), 116);
    assert_eq!(stats.frames_grouped(), 108);
    assert_eq!(stats.drops_total(), 8);
    assert_eq!(stats.dropped_by(0, DropReason::Unmatched), 3);
    assert_eq!(stats.dropped_by(0, DropReason::Incomplete), 1);
    assert_eq!(stats.dropped_by(1, DropReason::Unmatched), 1);
    assert_eq!(stats.dropped_by(2, DropReason::Unmatched), 3);
    assert_eq!(stats.dropped(DropReason::Stale), 0);
    assert_conserved(&g, 3);
}

#[test]
fn partial_emits_the_groups_a_lost_frame_leaves_short() {
    // The same recording: the three frames camera 1 lost and the one camera 2 lost each leave
    // a short group, and nothing is dropped.
    let config = GroupConfig::new(P / 2)
        .policy(GroupPolicy::Partial)
        .deadline_ns(50 * MS)
        .min_members(2);
    let mut g = three_cameras(config);
    let groups = feed(&mut g, &[CAM30_0, CAM30_1, CAM30_2]);
    let partial = groups.iter().filter(|g| !g.1).count();
    assert_eq!(groups.len(), 40);
    assert_eq!(partial, 4);
    assert!(groups.iter().all(|g| g.3 >= 2));
    assert_eq!(g.stats().drops_total(), 0);
    assert_conserved(&g, 3);
}

#[test]
fn slowest_pairs_60_fps_with_30_fps() {
    // 60 fps and 30 fps, 1 ms apart. One group per 30 fps frame; each 60 fps frame in between is
    // Unmatched, so half of all frames are grouped.
    let mut g: Grouper<u64, 2> = Grouper::new(GroupConfig::new(P / 4).rate(RateMatch::Slowest));
    g.add_camera().unwrap();
    g.add_camera().unwrap();
    assert_eq!(feed2(&mut g, &[RATE_0, RATE_1]), 40);
    let stats = g.stats();
    assert_eq!(stats.groups_complete(), 40);
    assert_eq!(stats.dropped_by(0, DropReason::Unmatched), 40);
    assert_eq!(stats.dropped_by(1, DropReason::Unmatched), 0);
    assert!((stats.match_rate().unwrap() - 2.0 / 3.0).abs() < 1e-9);
    assert_conserved(&g, 2);
}

#[test]
fn nearest_pairs_60_fps_with_30_fps() {
    // Nearest anchors on the oldest frame and pairs to the nearest one of the other camera. The
    // same 40 pairs come out here, the 60 fps frames in between are Unmatched.
    let mut g: Grouper<u64, 2> = Grouper::new(GroupConfig::new(P / 4));
    g.add_camera().unwrap();
    g.add_camera().unwrap();
    assert_eq!(feed2(&mut g, &[RATE_0, RATE_1]), 40);
    let stats = g.stats();
    assert_eq!(stats.groups_complete(), 40);
    assert_eq!(stats.dropped_by(0, DropReason::Unmatched), 40);
    assert_conserved(&g, 2);
}
