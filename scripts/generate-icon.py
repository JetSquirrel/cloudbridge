#!/usr/bin/env python3
"""Write every icon format the build uses from assets/icon.svg.

The SVG is the source: a rounded tile, one path for the bridge and a circle
for each of the dots above it, the bills it carries. This script draws those
itself, so it needs only Pillow and reads nothing else from the file.

    python3 scripts/generate-icon.py      # run from the repo root

Writes assets/icon.png (1024, on the macOS icon grid with its 100px margin),
assets/icon.icns, assets/icon.ico (the tile filling the square, 16-256) and
docs/icon.png (256, the site's favicon).
"""
import re

from PIL import Image, ImageChops, ImageDraw

SUPERSAMPLE = 4
CURVE_STEPS = 48


def read_source(path="assets/icon.svg"):
    text = open(path, encoding="utf-8").read()
    rect = re.search(r'<rect x="(\d+)" y="(\d+)" width="(\d+)" height="(\d+)" rx="(\d+)" fill="(#\w+)"', text)
    mark = re.search(r'<path fill="(#\w+)" fill-rule="evenodd" d="([^"]+)"', text)
    circles = [
        (float(cx), float(cy), float(r), fill)
        for cx, cy, r, fill in re.findall(r'<circle cx="([\d.]+)" cy="([\d.]+)" r="([\d.]+)" fill="(#\w+)"', text)
    ]
    x, y, w, h, r = (int(v) for v in rect.group(1, 2, 3, 4, 5))
    return (x, y, x + w, y + h), r, rect.group(6), mark.group(1), mark.group(2), circles


def flatten(d):
    """The subpaths of an absolute M/L/C/Z path, as lists of points."""
    subpaths, at = [], None
    for cmd, args in re.findall(r"([MLCZ])([^MLCZ]*)", d):
        n = [float(v) for v in args.split()]
        if cmd == "M":
            at = (n[0], n[1])
            subpaths.append([at])
        elif cmd == "L":
            at = (n[0], n[1])
            subpaths[-1].append(at)
        elif cmd == "C":
            x0, y0 = at
            for i in range(1, CURVE_STEPS + 1):
                t = i / CURVE_STEPS
                u = 1 - t
                a, b, c, e = u * u * u, 3 * u * u * t, 3 * u * t * t, t * t * t
                subpaths[-1].append(
                    (a * x0 + b * n[0] + c * n[2] + e * n[4], a * y0 + b * n[1] + c * n[3] + e * n[5])
                )
            at = (n[4], n[5])
    return subpaths


def render(size, source):
    """The icon at `size` px on the macOS grid, with a transparent margin."""
    tile, radius, tile_fill, mark_fill, d, circles = source
    k = size * SUPERSAMPLE / 1024
    w = round(1024 * k)
    out = Image.new("RGBA", (w, w), (0, 0, 0, 0))
    draw = ImageDraw.Draw(out)
    draw.rounded_rectangle([v * k for v in tile], radius=radius * k, fill=tile_fill)
    # Even-odd, so that a subpath inside another is a hole.
    mask = Image.new("1", (w, w), 0)
    for points in flatten(d):
        layer = Image.new("1", (w, w), 0)
        ImageDraw.Draw(layer).polygon([(x * k, y * k) for x, y in points], fill=1)
        mask = ImageChops.logical_xor(mask, layer)
    out.paste(mark_fill, (0, 0), mask.convert("L"))
    for cx, cy, r, fill in circles:
        draw.ellipse([(cx - r) * k, (cy - r) * k, (cx + r) * k, (cy + r) * k], fill=fill)
    return out.resize((size, size), Image.LANCZOS)


def full_bleed(size, source):
    """The tile filling the whole square, as Windows icons are drawn."""
    tile = source[0]
    big = round(size * 1024 / (tile[2] - tile[0]))
    m = (big - size) // 2
    return render(big, source).crop((m, m, m + size, m + size))


def main():
    source = read_source()
    big = render(1024, source)
    big.save("assets/icon.png")
    big.save("assets/icon.icns")
    render(256, source).save("docs/icon.png")
    sizes = [16, 24, 32, 48, 64, 128, 256]
    images = [full_bleed(n, source) for n in sizes]
    images[-1].save("assets/icon.ico", sizes=[(n, n) for n in sizes], append_images=images[:-1])


if __name__ == "__main__":
    main()
