#!/usr/bin/env python3
"""Per-zone shading and colour spread from a quality.py result, against a reference session,
leaving out the zones where two reference sessions differ by more than 3% (moving or blinking
parts of the scene).

    zone_spread.py QUALITY_OUT_DIR REFERENCE REFERENCE_SECOND_SESSION

For each frame: the median of the per-zone luma, R/G and B/G ratios to the reference, half
the 5-95% range of the ratios normalised to that median, and the largest deviation.
"""
import json,sys,numpy as np
d=json.load(open(sys.argv[1]+'/quality.json'))
ref,ref2=sys.argv[2],sys.argv[3]
Z=lambda k,f: np.array(d[k][f])
stable=np.abs(Z(ref2,'zone_luma')/Z(ref,'zone_luma')-1)<0.03
stable&=np.abs(Z(ref2,'zone_rg')/Z(ref,'zone_rg')-1)<0.03
stable&=np.abs(Z(ref2,'zone_bg')/Z(ref,'zone_bg')-1)<0.03
print('stable zones', stable.sum(), 'of', stable.size)
for k in d:
    out=[k]
    for f in ['zone_luma','zone_rg','zone_bg']:
        r=(Z(k,f)/Z(ref,f))[stable]; m=np.median(r); rn=r/m
        out.append('%s med %.3f spread ±%.1f%% (max %.1f%%)'%(f[5:],m,50*(np.percentile(rn,95)-np.percentile(rn,5))*100/100,100*np.abs(rn-1).max()))
    print(' | '.join(out))
