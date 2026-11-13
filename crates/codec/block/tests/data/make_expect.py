#!/usr/bin/env python3
"""Writes the expectations for one H.264 fixture: python3 -I make_expect.py <stream> <out.expect>

Ground truth comes from outside the Rust parser:

* Syntax: element values are read from FFmpeg's CBS trace (`-bsf:v trace_headers`), which prints
  every element with its RBSP bit position. Nothing here reads the Rust code's output.
* Reference lists and marking: derived in this file from ITU-T H.264 8.2.4 (list initialisation
  and modification) and 8.2.5 (marking), written apart from the Rust code. The long-term paths are
  modelled, but none of these streams uses them; MMCO 5 and prediction weights for B slices are
  refused here.
* Output order: the display order is the order of the POCs. check_display.py confirms it against
  the frames that GStreamer's VA-API decoder outputs, pixel-matched to the encoder's source.
* Prediction weight tables are read from the trace's named elements (luma_weight_l0[0] and so on).

The .expect file has one line per slice in decode order, then the display order of the pictures:
  slice pic=K first=M nal=T ref=R kind=K frame_num=N poc=P n0=A n1=B l0=... l1=... mmco=...
        mod0=... mod1=... cabac=C data=D
  display pic=... pic=...
A picture is numbered in decode order; its slices share it. l0 and l1 name the pictures (their
numbers) that each slice's reference lists hold.
"""
import re
import subprocess
import sys

ELEMENT = re.compile(r"^(\d+)\s+(\S+)\s+[01]+\s*=\s*(-?\d+)$")
NAL = re.compile(r"^nal_unit_type: (\d+)\(")


def trace(path):
    cmd = ["ffmpeg", "-hide_banner", "-loglevel", "trace", "-i", path, "-c", "copy",
           "-bsf:v", "trace_headers", "-f", "null", "-"]
    err = subprocess.run(cmd, capture_output=True, text=True, check=True).stderr
    # Each NAL unit's header starts with forbidden_zero_bit at bit 0; the type is its element.
    nals = []
    for line in err.splitlines():
        if "[trace_headers" not in line:
            continue
        text = line.split("] ", 1)[1]
        m = ELEMENT.match(text.strip())
        if not m:
            continue
        name, value, pos = m.group(2), int(m.group(3)), int(m.group(1))
        if name == "forbidden_zero_bit":
            nals.append([0, []])
        if nals:
            nals[-1][1].append((name, value, pos))
            if name == "nal_unit_type":
                nals[-1][0] = value
    return [(nal, fields) for nal, fields in nals]


class Cursor:
    def __init__(self, fields):
        self.f = fields
        self.i = 0

    def take(self, name):
        got, value, pos = self.f[self.i]
        if got != name:
            raise SystemExit(f"expected {name}, trace has {got} at bit {pos}")
        self.i += 1
        return value, pos


    def peek(self):
        return self.f[self.i][0] if self.i < len(self.f) else None


def parameter_sets(nals):
    sps, pps = None, {}
    for nal, fields in nals:
        d = {}
        for name, value, _ in fields:
            d.setdefault(name, value)
        if nal == 7:
            sps = d
        elif nal == 8:
            pps[d["pic_parameter_set_id"]] = d
    return sps, pps


def list_mods(c, lst):
    """ref_pic_list_modification() for list lst, after its flag was set."""
    ops = []
    while True:
        idc, _ = c.take("modification_of_pic_nums_idc")
        if idc == 3:
            return ops
        val, _ = c.take("abs_diff_pic_num_minus1" if idc in (0, 1) else "long_term_pic_num")
        ops.append((idc, val))


def slice_records(nals, sps, pps_map):
    recs = []
    for nal, fields in nals:
        if nal not in (1, 5):
            continue
        c = Cursor(fields)
        c.take("forbidden_zero_bit")
        ref_idc, _ = c.take("nal_ref_idc")
        c.take("nal_unit_type")
        first_mb, _ = c.take("first_mb_in_slice")
        st, _ = c.take("slice_type")
        pps_id, _ = c.take("pic_parameter_set_id")
        pps = pps_map[pps_id]
        frame_num, _ = c.take("frame_num")
        if nal == 5:
            c.take("idr_pic_id")
        poc_lsb = None
        if sps["pic_order_cnt_type"] == 0:
            poc_lsb, _ = c.take("pic_order_cnt_lsb")
            if pps.get("bottom_field_pic_order_in_frame_present_flag"):
                c.take("delta_pic_order_cnt_bottom")
        elif sps["pic_order_cnt_type"] == 2:
            # 8.2.1.3 with FrameNumOffset 0 (frame_num never wraps in these streams).
            poc_lsb = 2 * frame_num if ref_idc else 2 * frame_num - 1
        if pps.get("redundant_pic_cnt_present_flag"):
            raise SystemExit("redundant pictures are not in these streams")
        kind = ["P", "B", "I", "SP", "SI"][st % 5]
        if kind == "B":
            c.take("direct_spatial_mv_pred_flag")
        n = [0, 0]
        mods = [[], []]
        if kind in ("P", "SP", "B"):
            n[0] = pps["num_ref_idx_l0_default_active_minus1"] + 1
            n[1] = pps["num_ref_idx_l1_default_active_minus1"] + 1 if kind == "B" else 0
            if c.take("num_ref_idx_active_override_flag")[0]:
                n[0] = c.take("num_ref_idx_l0_active_minus1")[0] + 1
                if kind == "B":
                    n[1] = c.take("num_ref_idx_l1_active_minus1")[0] + 1
            if c.take("ref_pic_list_modification_flag_l0")[0]:
                mods[0] = list_mods(c, 0)
            if kind == "B" and c.take("ref_pic_list_modification_flag_l1")[0]:
                mods[1] = list_mods(c, 1)
        weights = None
        if (pps.get("weighted_pred_flag") and kind in ("P", "SP")) or (
            kind == "B" and pps.get("weighted_bipred_idc") == 1
        ):
            # 7.3.3.2: the denominators, then per active entry a luma and a chroma flag, each
            # followed by its weight and offset (chroma: Cb, then Cr). The trace gives values only.
            ld, _ = c.take("luma_log2_weight_denom")
            cd, _ = c.take("chroma_log2_weight_denom")
            weights = [f"d{ld}:{cd}"]
            for lst in (0, 1):
                if lst == 1 and kind != "B":
                    break
                parts = []
                for idx in range(n[lst]):
                    text = ""
                    if c.take(f"luma_weight_l{lst}_flag[{idx}]")[0]:
                        text += f"L{c.take(f'luma_weight_l{lst}[{idx}]')[0]}"
                        text += f":{c.take(f'luma_offset_l{lst}[{idx}]')[0]}"
                    else:
                        text += "L-"
                    if c.take(f"chroma_weight_l{lst}_flag[{idx}]")[0]:
                        vals = []
                        for j in range(2):
                            vals.append(c.take(f"chroma_weight_l{lst}[{idx}][{j}]")[0])
                            vals.append(c.take(f"chroma_offset_l{lst}[{idx}][{j}]")[0])
                        text += "C" + ":".join(map(str, vals))
                    else:
                        text += "C-"
                    parts.append(text)
                weights.append(",".join(parts))
            weights = ";".join(weights)
        mmco = []
        adaptive = 0
        ltr = 0
        if ref_idc != 0:
            if nal == 5:
                c.take("no_output_of_prior_pics_flag")
                ltr = c.take("long_term_reference_flag")[0]
            else:
                adaptive = c.take("adaptive_ref_pic_marking_mode_flag")[0]
                if adaptive:
                    while True:
                        op = c.take("memory_management_control_operation")[0]
                        if op == 0:
                            break
                        if op in (5,):
                            raise SystemExit("MMCO 5 is not modelled")
                        if op == 4:
                            mmco.append((op, c.take("max_long_term_frame_idx_plus1")[0]))
                        elif op == 1:
                            mmco.append((op, c.take("difference_of_pic_nums_minus1")[0]))
                        elif op == 2:
                            mmco.append((op, c.take("long_term_pic_num")[0]))
                        elif op == 3:
                            d = c.take("difference_of_pic_nums_minus1")[0]
                            mmco.append((op, (d, c.take("long_term_frame_idx")[0])))
                        elif op == 6:
                            mmco.append((op, c.take("long_term_frame_idx")[0]))
                        else:
                            raise SystemExit(f"MMCO {op}")
        # cabac_init_idc and slice_qp_delta follow; the header is not read past them.
        recs.append(dict(nal=nal, ref=ref_idc, kind=kind, st=st, frame_num=frame_num,
                         first_mb=first_mb, weights=weights,
                         poc=poc_lsb, n=n, mods=mods, mmco=mmco, adaptive=adaptive, ltr=ltr,
                         cabac=pps["entropy_coding_mode_flag"],
                         data=first_alignment_or_none(fields)))
    return recs


def first_alignment_or_none(fields):
    """The bit where the slice data starts: the first cabac_alignment_one_bit, when present."""
    for name, _, pos in fields:
        if name == "cabac_alignment_one_bit":
            return pos
    return None


# ---- the reference model (8.2.4 and 8.2.5), kept apart from the Rust tracker ----

class Ref:
    def __init__(self, pic, frame_num, poc):
        self.pic = pic
        self.frame_num = frame_num
        self.poc = poc
        self.long = None


def group_pictures(recs):
    """Slices of one picture: a slice with first_mb_in_slice 0 starts a new picture (no ASO)."""
    pics = []
    for r in recs:
        if r["first_mb"] == 0 or not pics:
            pics.append([r])
        else:
            pics[-1].append(r)
    return pics


def model(pics, sps):
    """The lists of every slice, and the display order of the pictures."""
    max_frame_num = 1 << (sps["log2_max_frame_num_minus4"] + 4)
    max_refs = max(sps["max_num_ref_frames"], 1)
    dpb = []
    slice_lists = []
    pocs = []
    for k, pic in enumerate(pics):
        head = pic[0]
        if head["nal"] == 5:
            dpb = []
        cur_num = head["frame_num"]
        poc = head["poc"] if head["poc"] is not None else 0
        pocs.append(poc)

        def wrap(fn):
            return fn - max_frame_num if fn > cur_num else fn

        for r in pic:
            shorts = [x for x in dpb if x.long is None]
            longs = sorted([x for x in dpb if x.long is not None], key=lambda x: x.long)
            lists = [[], []]
            if r["kind"] in ("P", "SP"):
                lists[0] = sorted(shorts, key=lambda x: -wrap(x.frame_num)) + longs
            elif r["kind"] == "B":
                before = sorted([x for x in shorts if x.poc < poc], key=lambda x: -x.poc)
                after = sorted([x for x in shorts if x.poc > poc], key=lambda x: x.poc)
                lists[0] = before + after + longs
                lists[1] = after + before + longs
                if len(lists[1]) > 1 and [x.pic for x in lists[0]] == [x.pic for x in lists[1]]:
                    lists[1][0], lists[1][1] = lists[1][1], lists[1][0]
            final = [[], []]
            for lst in (0, 1):
                n = r["n"][lst]
                if n == 0:
                    continue
                init = lists[lst][:n]
                work = init + [None] * (n + 1 - len(init))
                pred = cur_num
                idx = 0
                for idc, val in r["mods"][lst]:
                    if idc in (0, 1):
                        absd = val + 1
                        if idc == 0:
                            no_wrap = pred - absd
                            if no_wrap < 0:
                                no_wrap += max_frame_num
                        else:
                            no_wrap = pred + absd
                            if no_wrap >= max_frame_num:
                                no_wrap -= max_frame_num
                        pred = no_wrap
                        pic_num = no_wrap - max_frame_num if no_wrap > cur_num else no_wrap
                        target = next(x for x in shorts if wrap(x.frame_num) == pic_num)
                    else:
                        target = next(x for x in longs if x.long == val)
                    for c in range(n, idx, -1):
                        work[c] = work[c - 1]
                    work[idx] = target
                    idx += 1
                    nxt = idx
                    for c in range(idx, n + 1):
                        if work[c] is not target:
                            work[nxt] = work[c]
                            nxt += 1
                final[lst] = work[:n]
            slice_lists.append([[x.pic for x in final[0]], [x.pic for x in final[1]]])
        # Marking (8.2.5) once the picture is decoded, from its first slice's syntax.
        if head["ref"] != 0:
            cur = Ref(k, cur_num, poc)
            if head["nal"] == 5:
                dpb = []
                if head["ltr"]:
                    cur.long = 0
                dpb.append(cur)
            elif head["adaptive"]:
                for op, arg in head["mmco"]:
                    if op == 1:
                        pn = cur_num - (arg + 1)
                        dpb = [x for x in dpb if not (x.long is None and wrap(x.frame_num) == pn)]
                    elif op == 2:
                        dpb = [x for x in dpb if not (x.long == arg)]
                    elif op == 3:
                        d, idx = arg
                        pn = cur_num - (d + 1)
                        tgt = next(x for x in dpb if x.long is None and wrap(x.frame_num) == pn)
                        dpb = [x for x in dpb if x is tgt or x.long != idx]
                        tgt.long = idx
                    elif op == 4:
                        mx = arg - 1
                        dpb = [x for x in dpb if x.long is None or (mx >= 0 and x.long <= mx)]
                    elif op == 6:
                        dpb = [x for x in dpb if x.long != arg]
                        cur.long = arg
                dpb.append(cur)
            else:
                if len(dpb) >= max_refs:
                    victim = min([x for x in dpb if x.long is None],
                                 key=lambda x: wrap(x.frame_num))
                    dpb = [x for x in dpb if x is not victim]
                dpb.append(cur)
    display = sorted(range(len(pics)), key=lambda k: pocs[k])
    return slice_lists, display, pocs


def mmco_text(op, arg):
    """`op:arg`, with MMCO 3's two values as `d/idx`."""
    if op == 3:
        return f"3:{arg[0]}/{arg[1]}"
    return f"{op}:{arg}"


def main():
    stream, dest = sys.argv[1], sys.argv[2]
    nals = trace(stream)
    sps, pps_map = parameter_sets(nals)
    recs = slice_records(nals, sps, pps_map)
    pics = group_pictures(recs)
    lists, display, pocs = model(pics, sps)
    # The display order is the order of the POCs, which must be distinct within the stream.
    if len(set(pocs)) != len(pocs):
        raise SystemExit(f"POCs repeat: {pocs}")
    lines = []
    k = 0
    for pi, pic in enumerate(pics):
        for si, r in enumerate(pic):
            l0, l1 = lists[k]
            k += 1

            def fmt(xs):
                return ",".join(map(str, xs)) if xs else "-"

            lines.append(
                f"slice pic={pi} first={r['first_mb']} nal={r['nal']} ref={r['ref']} kind={r['kind']}"
                f" frame_num={r['frame_num']} poc={r['poc']} n0={r['n'][0]} n1={r['n'][1]}"
                f" l0={fmt(l0)} l1={fmt(l1)}"
                f" mmco={';'.join(mmco_text(op, a) for op, a in r['mmco']) or '-'}"
                f" mod0={';'.join(f'{i}:{v}' for i, v in r['mods'][0]) or '-'}"
                f" mod1={';'.join(f'{i}:{v}' for i, v in r['mods'][1]) or '-'}"
                f" wp={r['weights'] or '-'}"
                f" cabac={r['cabac']} data={r['data'] if r['data'] is not None else '-'}"
            )
    lines.append("display " + " ".join(f"pic={k}" for k in display))
    with open(dest, "w") as fh:
        fh.write("\n".join(lines) + "\n")
    print(f"{len(recs)} slices in {len(pics)} pictures, {sum(r['kind'] == 'B' for r in recs)} B slices, "
          f"{sum(1 for r in recs if r['mmco'])} with MMCO, "
          f"{sum(1 for r in recs if r['mods'][0] or r['mods'][1])} with list modification")


if __name__ == "__main__":
    main()
