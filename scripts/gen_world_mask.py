#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-or-later
# Copyright (C) Opinsys Oy 2026
"""Generate a 1-bit land/sea raster mask for the timezone world map.

The output is a packed-bit bitmap: ``MAP_W`` x ``MAP_H`` cells, row-major,
top row = +90 deg latitude (north), left column = -180 deg longitude. Each
byte holds 8 cells, MSB first; each row is a whole number of bytes. A set bit
is land.

Source: Natural Earth 1:110m land (public domain), a 127-polygon vector set.
The script downloads it on demand and writes ``src/tui/world_mask.bin``.

The mask is committed; this generator only needs to run when the coastline
source is updated. No runtime/build dependency on this script or on Natural
Earth data: the installer embeds the .bin via ``include_bytes!``.
"""
import json
import sys
import urllib.request
from pathlib import Path

from PIL import Image, ImageDraw

NE_LAND_URL = (
    "https://raw.githubusercontent.com/nvkelso/natural-earth-vector/"
    "master/geojson/ne_110m_land.geojson"
)

MAP_W = 720
MAP_H = 360

ROOT = Path(__file__).resolve().parents[1]
GEOJSON = Path("/tmp/ne110_land.geojson")
OUT = ROOT / "src" / "tui" / "world_mask.bin"


def lonlat_to_xy(lon, lat):
    return ((lon + 180.0) / 360.0 * MAP_W, (90.0 - lat) / 180.0 * MAP_H)


def load_features(path):
    if not path.exists():
        print(f"downloading {NE_LAND_URL} -> {path}", file=sys.stderr)
        urllib.request.urlretrieve(NE_LAND_URL, path)
    data = json.load(open(path))
    return data["features"]


def rasterize(features):
    img = Image.new("1", (MAP_W, MAP_H), 0)  # 0 = sea
    draw = ImageDraw.Draw(img)
    for feat in features:
        rings = feat["geometry"]["coordinates"]  # [outer, hole, ...]
        outer = [lonlat_to_xy(lon, lat) for lon, lat in rings[0]]
        if len(outer) >= 3:
            draw.polygon(outer, fill=1)
        for hole in rings[1:]:
            pts = [lonlat_to_xy(lon, lat) for lon, lat in hole]
            if len(pts) >= 3:
                draw.polygon(pts, fill=0)
    return img


def preview(img, cols=90, rows=24):
    w, h = img.size
    sx, sy = w / cols, h / rows
    chars = " .:-=+*#%@"
    lines = []
    for r in range(rows):
        line = []
        for c in range(cols):
            x0 = int(c * sx)
            y0 = int(r * sy)
            cnt = 0
            for yy in range(int(sy) + 1):
                for xx in range(int(sx) + 1):
                    px = min(x0 + xx, w - 1)
                    py = min(y0 + yy, h - 1)
                    cnt += img.getpixel((px, py))
            area = max(1, (int(sy) + 1) * (int(sx) + 1))
            line.append(chars[min(len(chars) - 1, cnt * len(chars) // area)])
        lines.append("".join(line))
    return "\n".join(lines)


def main():
    feats = load_features(GEOJSON)
    print(f"features: {len(feats)}", file=sys.stderr)
    img = rasterize(feats)
    print(preview(img), file=sys.stderr)
    raw = img.tobytes("raw", "1")
    expect = (MAP_W + 7) // 8 * MAP_H
    assert len(raw) == expect, f"raw={len(raw)} expect={expect}"
    OUT.write_bytes(raw)
    print(f"wrote {OUT} ({len(raw)} bytes, {MAP_W}x{MAP_H})", file=sys.stderr)


if __name__ == "__main__":
    main()
