#!/usr/bin/env python3
"""Pure-stdlib PNG decoder + region locator + cropper (no Pillow/ImageMagick).

The dev box has no image libraries installed, so this is the workhorse for
turning a screenshot into coordinates: find a UI element by color, find text
rows by brightness, sample pixels, and crop a focused region back out to a
new PNG (e.g. to attach legible before/after shots to a PR).

Subcommands:
  info FILE                       -> "W H bitdepth colortype interlace"
  pixel FILE X Y                  -> "R G B" at (X,Y)
  hscan FILE Y [STEP]             -> colors along row Y every STEP px (default 40)
  vscan FILE X [STEP]             -> colors down column X every STEP px
  textscan FILE x0 x1 y0 y1 THR   -> rows in the box with a >THR-bright pixel,
                                     as "y=.. count=.. xrange=lo..hi" (find text)
  findcolor FILE R G B TOL        -> "minx miny maxx maxy count cx cy" bounding
                                     box of pixels within Chebyshev TOL of RGB
  crop FILE OUT x y w h           -> write the region as an 8-bit RGB PNG

Handles 8-bit color type 2 (RGB) and 6 (RGBA), non-interlaced — what
cosmic-screenshot and `roostctl screenshot` emit.
"""
import sys
import zlib
import struct


def load(path):
    """Decode a PNG to (width, height, bytes_per_pixel, pixel_bytes).

    Reverses each scanline's filter (None/Sub/Up/Average/Paeth) so the
    returned buffer is raw top-to-bottom rows of `width*bpp` bytes.
    """
    with open(path, "rb") as f:
        data = f.read()
    assert data[:8] == b"\x89PNG\r\n\x1a\n", "not a PNG"
    pos = 8
    width = height = bitdepth = colortype = interlace = None
    idat = bytearray()
    while pos < len(data):
        (length,) = struct.unpack(">I", data[pos:pos + 4])
        ctype = data[pos + 4:pos + 8]
        body = data[pos + 8:pos + 8 + length]
        pos += 12 + length  # 4 len + 4 type + body + 4 crc
        if ctype == b"IHDR":
            width, height, bitdepth, colortype, _comp, _filt, interlace = \
                struct.unpack(">IIBBBBB", body)
        elif ctype == b"IDAT":
            idat += body
        elif ctype == b"IEND":
            break
    assert interlace == 0, "interlaced PNG unsupported"
    assert bitdepth == 8, f"bitdepth {bitdepth} unsupported"
    assert colortype in (2, 6), f"colortype {colortype} unsupported"
    bpp = 3 if colortype == 2 else 4
    raw = zlib.decompress(bytes(idat))
    stride = width * bpp
    out = bytearray(height * stride)
    prev = bytearray(stride)
    rp = 0
    for y in range(height):
        ft = raw[rp]
        rp += 1
        line = bytearray(raw[rp:rp + stride])
        rp += stride
        if ft == 1:    # Sub
            for i in range(bpp, stride):
                line[i] = (line[i] + line[i - bpp]) & 0xFF
        elif ft == 2:  # Up
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 0xFF
        elif ft == 3:  # Average
            for i in range(stride):
                a = line[i - bpp] if i >= bpp else 0
                line[i] = (line[i] + ((a + prev[i]) >> 1)) & 0xFF
        elif ft == 4:  # Paeth
            for i in range(stride):
                a = line[i - bpp] if i >= bpp else 0
                b = prev[i]
                c = prev[i - bpp] if i >= bpp else 0
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[i] = (line[i] + pr) & 0xFF
        out[y * stride:(y + 1) * stride] = line
        prev = line
    return width, height, bpp, out


def write_png(path, w, h, rgb):
    """Write an 8-bit RGB (colortype 2) PNG from a flat w*h*3 bytes buffer."""
    def chunk(typ, data):
        return (struct.pack(">I", len(data)) + typ + data
                + struct.pack(">I", zlib.crc32(typ + data) & 0xffffffff))
    raw = bytearray()
    stride = w * 3
    for y in range(h):
        raw.append(0)  # filter type 0 (None)
        raw += rgb[y * stride:(y + 1) * stride]
    idat = zlib.compress(bytes(raw), 9)
    ihdr = struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)
    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr)
                + chunk(b"IDAT", idat) + chunk(b"IEND", b""))


def main():
    cmd = sys.argv[1]
    path = sys.argv[2]
    if cmd == "info":
        with open(path, "rb") as f:
            d = f.read(33)
        w, h, bd, ct, _, _, il = struct.unpack(">IIBBBBB", d[16:29])
        print(w, h, bd, ct, il)
        return
    w, h, bpp, px = load(path)
    if cmd == "pixel":
        x, y = int(sys.argv[3]), int(sys.argv[4])
        o = (y * w + x) * bpp
        print(px[o], px[o + 1], px[o + 2])
    elif cmd == "hscan":
        y = int(sys.argv[3])
        step = int(sys.argv[4]) if len(sys.argv) > 4 else 40
        for x in range(0, w, step):
            o = (y * w + x) * bpp
            print(f"x={x}:{px[o]},{px[o+1]},{px[o+2]}", end="  ")
        print()
    elif cmd == "vscan":
        x = int(sys.argv[3])
        step = int(sys.argv[4]) if len(sys.argv) > 4 else 40
        for y in range(0, h, step):
            o = (y * w + x) * bpp
            print(f"y={y}:{px[o]},{px[o+1]},{px[o+2]}", end="  ")
        print()
    elif cmd == "crop":
        out = sys.argv[3]
        cx, cy, cw, ch = (int(sys.argv[4]), int(sys.argv[5]),
                          int(sys.argv[6]), int(sys.argv[7]))
        cw = min(cw, w - cx)
        ch = min(ch, h - cy)
        stride = w * bpp
        rgb = bytearray(cw * ch * 3)
        for ry in range(ch):
            src = (cy + ry) * stride + cx * bpp
            dst = ry * cw * 3
            for rx in range(cw):
                s = src + rx * bpp
                rgb[dst + rx * 3] = px[s]
                rgb[dst + rx * 3 + 1] = px[s + 1]
                rgb[dst + rx * 3 + 2] = px[s + 2]
        write_png(out, cw, ch, rgb)
        print(f"wrote {out} {cw}x{ch}")
    elif cmd == "textscan":
        x0, x1, y0, y1, th = (int(sys.argv[3]), int(sys.argv[4]),
                              int(sys.argv[5]), int(sys.argv[6]), int(sys.argv[7]))
        stride = w * bpp
        for y in range(y0, y1):
            base = y * stride
            cnt = 0
            lo = hi = None
            for x in range(x0, x1):
                o = base + x * bpp
                if max(px[o], px[o + 1], px[o + 2]) > th:
                    cnt += 1
                    if lo is None:
                        lo = x
                    hi = x
            if cnt > 0:
                print(f"y={y} count={cnt} xrange={lo}..{hi}")
    elif cmd == "findcolor":
        tr, tg, tb, tol = (int(sys.argv[3]), int(sys.argv[4]),
                           int(sys.argv[5]), int(sys.argv[6]))
        minx = miny = 1 << 30
        maxx = maxy = -1
        count = sx = sy = 0
        stride = w * bpp
        for y in range(h):
            base = y * stride
            for x in range(w):
                o = base + x * bpp
                if (abs(px[o] - tr) <= tol and abs(px[o + 1] - tg) <= tol
                        and abs(px[o + 2] - tb) <= tol):
                    minx = min(minx, x)
                    maxx = max(maxx, x)
                    miny = min(miny, y)
                    maxy = max(maxy, y)
                    sx += x
                    sy += y
                    count += 1
        if count == 0:
            print("none")
        else:
            print(minx, miny, maxx, maxy, count, sx // count, sy // count)
    else:
        sys.exit(f"unknown command: {cmd}")


if __name__ == "__main__":
    main()
