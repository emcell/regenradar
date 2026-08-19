#!/usr/bin/env python3
"""Generate the PWA icon set.

Pure stdlib on purpose — no image library, no build-time dependency. Everything is
drawn at 4x and box-filtered down, which is enough antialiasing for an app icon.

Run from the `web/` directory:  python3 tools/make-icons.py
"""

import math
import os
import struct
import zlib

SS = 4  # supersampling factor

BG = (0x0B, 0x10, 0x17)
RING = (0x49, 0xB7, 0xFF)
DROP_TOP = (0x7A, 0xE7, 0xFF)
DROP_BOT = (0x2C, 0x7C, 0xE0)


def lerp(a, b, t):
    return tuple(round(x + (y - x) * t) for x, y in zip(a, b))


def over(dst, src, alpha):
    """Source-over composite of an opaque colour at `alpha`."""
    return tuple(round(d + (s - d) * alpha) for d, s in zip(dst, src))


def draw(size, maskable):
    """Render one icon at `size` px, returning RGBA rows."""
    n = size * SS
    # Maskable icons are cropped to a circle by the launcher, so the artwork has to
    # sit inside the safe zone and the background has to bleed to the edges.
    scale = 0.62 if maskable else 0.80
    corner = 0.0 if maskable else 0.235

    cx, cy = n / 2.0, n / 2.0
    unit = n * scale  # nominal artwork size

    ring_radii = [0.215, 0.315, 0.415]
    ring_alpha = [0.95, 0.55, 0.28]
    ring_w = unit * 0.052

    drop_r = unit * 0.135
    drop_cy = cy + unit * 0.055
    drop_top = drop_cy - drop_r * 2.25

    px = bytearray(n * n * 4)

    for y in range(n):
        fy = y + 0.5
        for x in range(n):
            fx = x + 0.5

            # --- background plate, optionally rounded ---
            a_bg = 1.0
            if corner > 0.0:
                r = corner * n
                dx = max(r - fx, fx - (n - r), 0.0)
                dy = max(r - fy, fy - (n - r), 0.0)
                if dx > 0.0 and dy > 0.0:
                    d = math.hypot(dx, dy)
                    a_bg = max(0.0, min(1.0, r - d + 0.5))
            if a_bg <= 0.0:
                continue

            col = BG

            # --- radar rings, drawn as a sweep from the lower left ---
            d = math.hypot(fx - cx, fy - cy)
            ang = math.atan2(fy - cy, fx - cx)
            for rr, ra in zip(ring_radii, ring_alpha):
                radius = unit * rr
                edge = abs(d - radius)
                if edge > ring_w:
                    continue
                cov = max(0.0, min(1.0, (ring_w / 2 - edge) / (ring_w / 2) * 2.0))
                if cov <= 0.0:
                    continue
                # Fade the arc out over the top-right quadrant so it reads as a
                # sweep rather than a flat bullseye.
                sweep = (math.cos(ang - 2.5) + 1.0) / 2.0
                col = over(col, RING, cov * ra * (0.25 + 0.75 * sweep))

            # --- droplet ---
            if drop_top <= fy <= drop_cy + drop_r:
                if fy >= drop_cy:
                    half = math.sqrt(max(0.0, drop_r**2 - (fy - drop_cy) ** 2))
                else:
                    t = (fy - drop_top) / (drop_cy - drop_top)
                    half = drop_r * (t**0.85)
                cov = max(0.0, min(1.0, half - abs(fx - cx) + 0.5))
                if cov > 0.0:
                    g = (fy - drop_top) / (drop_cy + drop_r - drop_top)
                    col = over(col, lerp(DROP_TOP, DROP_BOT, g), cov)

            i = (y * n + x) * 4
            px[i] = col[0]
            px[i + 1] = col[1]
            px[i + 2] = col[2]
            px[i + 3] = round(a_bg * 255)

    return downsample(px, n, size)


def downsample(px, n, size):
    out = bytearray(size * size * 4)
    area = SS * SS
    for y in range(size):
        for x in range(size):
            acc = [0, 0, 0, 0]
            for sy in range(SS):
                row = (y * SS + sy) * n
                for sx in range(SS):
                    i = (row + x * SS + sx) * 4
                    a = px[i + 3]
                    # Weight colour by alpha so transparent corners do not darken
                    # the edge pixels.
                    acc[0] += px[i] * a
                    acc[1] += px[i + 1] * a
                    acc[2] += px[i + 2] * a
                    acc[3] += a
            j = (y * size + x) * 4
            if acc[3] > 0:
                out[j] = acc[0] // acc[3]
                out[j + 1] = acc[1] // acc[3]
                out[j + 2] = acc[2] // acc[3]
            out[j + 3] = acc[3] // area
    return out


def write_png(path, rgba, size):
    raw = bytearray()
    stride = size * 4
    for y in range(size):
        raw.append(0)  # filter type 0
        raw += rgba[y * stride : (y + 1) * stride]

    def chunk(tag, data):
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(bytes(raw), 9))
    png += chunk(b"IEND", b"")
    with open(path, "wb") as fh:
        fh.write(png)
    print(f"{path}  {size}x{size}  {len(png) / 1024:.1f} KB")


def main():
    out = os.path.join(os.path.dirname(__file__), "..", "public", "icons")
    os.makedirs(out, exist_ok=True)
    targets = [
        ("icon-192.png", 192, False),
        ("icon-512.png", 512, False),
        ("icon-maskable-512.png", 512, True),
        ("apple-touch-icon.png", 180, True),
        ("favicon.png", 64, False),
    ]
    for name, size, maskable in targets:
        write_png(os.path.join(out, name), draw(size, maskable), size)


if __name__ == "__main__":
    main()
