#!/usr/bin/env python3
"""Field-by-field diff of two PiSP back end configs: libcamera's JSON dump
(LIBCAMERA_RPI_PISP_CONFIG_DUMP=<file>, libpisp's GetJsonConfig) against a raw
pisp_be_tiles_config (native-pipeline pisp writes pisp-be-config-{first,last}.bin).

    be_config_diff.py LIBCAMERA.json OURS.bin [BLOCK_TO_SKIP...]

be_fields.json is libpisp's field table (backend_debug.cpp, BSD-2-Clause, Raspberry Pi Ltd)
with the offsets its compiler gives: [block, field, byte offset, size, count]. Note that
libpisp lists lsc.lut_packed as 33 entries, so its dump holds only the first row of the
33x33 table.
"""
import json, os, sys
F=json.load(open(os.path.join(os.path.dirname(os.path.abspath(__file__)), 'be_fields.json')))
def rd(b,off,size):
    return int.from_bytes(b[off:off+size],'little')
def ours(path):
    b=open(path,'rb').read()
    cfg={}
    for blk,name,off,size,num in F['config']:
        v=[rd(b,off+i*size,size) for i in range(num)]
        cfg.setdefault(blk,{})[name]=v[0] if num==1 else v
    nt=rd(b,F['num_tiles_off'],4)
    tiles=[]
    for t in range(nt):
        base=F['tiles_off']+t*F['tile_size']; d={}
        for name,off,size,num in F['tiles']:
            v=[rd(b,base+off+i*size,size) for i in range(num)]
            d[name]=v[0] if num==1 else v
        tiles.append(d)
    return cfg,tiles
def lc(path):
    d=json.load(open(path)); cfg={}
    for x in d['config']: cfg.update(x)
    return cfg,d['tiles']
if __name__=='__main__':
    a,at=lc(sys.argv[1]); b,bt=ours(sys.argv[2])
    skip=set(sys.argv[3:])
    for blk in a:
        for k in a[blk]:
            x,y=a[blk][k],b[blk][k]
            if x!=y and blk not in skip:
                print(f'{blk}.{k}: lc={x}\n   ours={y}')
    print('tiles', len(at), len(bt))
    for i,(x,y) in enumerate(zip(at,bt)):
        for k in x:
            if x[k]!=y[k]: print(f' tile{i}.{k}: lc={x[k]} ours={y[k]}')
