#!/usr/bin/env python3
"""Writes the glTF fixtures of the simulated camera tests (python3 -I make_sim_scene.py DIR):

sim_scene.gltf: a camera at the origin looking along -Z (a 0.9 rad vertical field of view), a
1 m square 3 m ahead textured with an 8x8 black and white checker (nearest), a grey backdrop
5 m ahead, `truth:` points at the square's corners (top left, top right, bottom right, bottom
left as seen) and one behind the square (`truth:hidden`).
sim_scene_moving.gltf: the same with the camera moving 1 m along +X and back over 4 s.
"""
import base64, json, struct, sys, zlib

def png(w, h, rows):
    raw = b"".join(b"\0" + bytes(r) for r in rows)
    def chunk(t, d):
        c = struct.pack(">I", len(d)) + t + d
        return c + struct.pack(">I", zlib.crc32(t + d) & 0xffffffff)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 0, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))

def scene(moving):
    # Square: corners (-0.5, 0.5), (0.5, 0.5), (0.5, -0.5), (-0.5, -0.5) at z = -3; uv (0,0) top left.
    pos = [-0.5, 0.5, -3, 0.5, 0.5, -3, 0.5, -0.5, -3, -0.5, -0.5, -3,
           -4, 3, -5, 4, 3, -5, 4, -3, -5, -4, -3, -5]
    uv = [0, 0, 1, 0, 1, 1, 0, 1] * 2
    idx = [0, 3, 2, 0, 2, 1, 0, 3, 2, 0, 2, 1]
    blob = struct.pack("<24f", *pos) + struct.pack("<16f", *uv) + struct.pack("<12H", *idx)
    views = [{"buffer": 0, "byteOffset": 0, "byteLength": 96},
             {"buffer": 0, "byteOffset": 96, "byteLength": 64},
             {"buffer": 0, "byteOffset": 160, "byteLength": 24}]
    acc = [{"bufferView": 0, "componentType": 5126, "count": 4, "type": "VEC3", "min": [-0.5, -0.5, -3], "max": [0.5, 0.5, -3]},
           {"bufferView": 0, "byteOffset": 48, "componentType": 5126, "count": 4, "type": "VEC3", "min": [-4, -3, -5], "max": [4, 3, -5]},
           {"bufferView": 1, "componentType": 5126, "count": 4, "type": "VEC2"},
           {"bufferView": 1, "byteOffset": 32, "componentType": 5126, "count": 4, "type": "VEC2"},
           {"bufferView": 2, "componentType": 5123, "count": 6, "type": "SCALAR"},
           {"bufferView": 2, "byteOffset": 12, "componentType": 5123, "count": 6, "type": "SCALAR"}]
    anim = []
    if moving:
        times = struct.pack("<3f", 0, 2, 4)
        trans = struct.pack("<9f", 0, 0, 0, 1, 0, 0, 0, 0, 0)
        off = len(blob)
        blob += times + trans
        views += [{"buffer": 0, "byteOffset": off, "byteLength": 12},
                  {"buffer": 0, "byteOffset": off + 12, "byteLength": 36}]
        acc += [{"bufferView": 3, "componentType": 5126, "count": 3, "type": "SCALAR", "min": [0], "max": [4]},
                {"bufferView": 4, "componentType": 5126, "count": 3, "type": "VEC3"}]
        anim = [{"channels": [{"sampler": 0, "target": {"node": 0, "path": "translation"}}],
                 "samplers": [{"input": 6, "output": 7, "interpolation": "LINEAR"}]}]
    checker = [[255 if (x + y) % 2 == 0 else 0 for x in range(8)] for y in range(8)]
    tex = base64.b64encode(png(8, 8, checker)).decode()
    corners = [("tl", -0.5, 0.5), ("tr", 0.5, 0.5), ("br", 0.5, -0.5), ("bl", -0.5, -0.5)]
    nodes = [{"name": "camera", "camera": 0},
             {"name": "square", "mesh": 0},
             {"name": "backdrop", "mesh": 1}]
    nodes += [{"name": f"truth:square/{n}", "translation": [x, y, -3]} for n, x, y in corners]
    nodes += [{"name": "truth:hidden", "translation": [0, 0, -5]}]
    doc = {
        "asset": {"version": "2.0", "generator": "styx make_sim_scene.py"},
        "scene": 0,
        "scenes": [{"nodes": list(range(len(nodes)))}],
        "nodes": nodes,
        "cameras": [{"type": "perspective", "perspective": {"yfov": 0.9, "znear": 0.05, "aspectRatio": 1.6}}],
        "meshes": [{"primitives": [{"attributes": {"POSITION": 0, "TEXCOORD_0": 2}, "indices": 4, "material": 0}]},
                   {"primitives": [{"attributes": {"POSITION": 1, "TEXCOORD_0": 3}, "indices": 5, "material": 1}]}],
        "materials": [{"pbrMetallicRoughness": {"baseColorTexture": {"index": 0}}},
                      {"pbrMetallicRoughness": {"baseColorFactor": [0.2, 0.2, 0.2, 1]}}],
        "textures": [{"source": 0, "sampler": 0}],
        "samplers": [{"magFilter": 9728, "minFilter": 9728}],
        "images": [{"uri": "data:image/png;base64," + tex}],
        "buffers": [{"byteLength": len(blob), "uri": "data:application/octet-stream;base64," + base64.b64encode(blob).decode()}],
        "bufferViews": views,
        "accessors": acc,
    }
    if anim:
        doc["animations"] = anim
    return json.dumps(doc, indent=1)

out = sys.argv[1] if len(sys.argv) > 1 else "."
open(f"{out}/sim_scene.gltf", "w").write(scene(False))
open(f"{out}/sim_scene_moving.gltf", "w").write(scene(True))
