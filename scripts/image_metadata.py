#!/usr/bin/env python3
"""Scan or strip location/identity metadata in images before they are published.

Why this exists: the v0.4.0 release verified "EXIF stripped" with
`mdls -name kMDItemLatitude`. `mdls` reads the Spotlight index, not the file, and
returns `(null)` for anything Spotlight has not indexed — so it reported clean on
files that still carried full GPS. Eighteen photos reached the public mirror with
sub-arcsecond home coordinates. Parse the container; never trust an indexer.

    python3 scripts/image_metadata.py scan  docs/images      # exit 1 if any GPS
    python3 scripts/image_metadata.py strip docs/images      # rewrite in place

Camera-named `IMG_*` files are skipped by default: they are the private originals,
never published, and the `pre-push` hook blocks them by name. Pass `--include-raw`
to audit them too.

Stripping is lossless: JPEG entropy-coded scan data is untouched and only whole
APP segments are dropped. APP0/JFIF and the APP2 ICC profile are kept (no
location, and dropping ICC shifts colour). A non-default Orientation is preserved
by re-emitting a minimal Exif block holding that one tag, so images do not rotate.
"""

import struct
import sys
import pathlib

GPS_IFD = 0x8825
ORIENTATION = 0x0112
NAMES = {
    0x010F: "Make", 0x0110: "Model", 0x0132: "DateTime", 0x9003: "DateTimeOriginal",
    0x8825: "GPSInfo", 0x013B: "Artist", 0x8298: "Copyright", 0x0112: "Orientation",
}


def _tiff_ifd0(buf):
    """Tags present in IFD0 of a TIFF/Exif block, as {tag: int_value}."""
    out = {}
    if len(buf) < 8:
        return out
    endian = "<" if buf[:2] == b"II" else ">" if buf[:2] == b"MM" else None
    if endian is None:
        return out
    try:
        off = struct.unpack_from(endian + "I", buf, 4)[0]
        if off <= 0 or off + 2 > len(buf):
            return out
        for i in range(struct.unpack_from(endian + "H", buf, off)[0]):
            p = off + 2 + i * 12
            if p + 12 > len(buf):
                break
            tag, typ, _cnt = struct.unpack_from(endian + "HHI", buf, p)
            # SHORT values sit in the high half of the 4-byte value field on a
            # big-endian file; read them at their true width, not as a u32.
            val = (struct.unpack_from(endian + "H", buf, p + 8)[0] if typ == 3
                   else struct.unpack_from(endian + "I", buf, p + 8)[0])
            out[tag] = val
    except struct.error:
        pass
    return out


def _jpeg_segments(d):
    """Yield (marker, full_segment_bytes, is_trailing_scan)."""
    i = 2
    while i + 4 <= len(d):
        if d[i] != 0xFF:
            break
        m = d[i + 1]
        if m in (0xD8, 0xD9) or 0xD0 <= m <= 0xD7:
            yield m, d[i:i + 2], False
            i += 2
            continue
        if m == 0xDA:                      # start of scan — rest is entropy data
            yield m, d[i:], True
            return
        ln = struct.unpack_from(">H", d, i + 2)[0]
        yield m, d[i:i + 2 + ln], False
        i += 2 + ln


def _payload(seg):
    return seg[4:]


def _minimal_exif(orientation):
    """A 34-byte Exif APP1 carrying Orientation and nothing else."""
    tiff = b"MM\x00*\x00\x00\x00\x08" + struct.pack(">H", 1)
    tiff += struct.pack(">HHI", ORIENTATION, 3, 1) + struct.pack(">HH", orientation, 0)
    tiff += struct.pack(">I", 0)
    body = b"Exif\x00\x00" + tiff
    return b"\xff\xe1" + struct.pack(">H", len(body) + 2) + body


def scan_jpeg(d):
    hits = {}
    for m, seg, _ in _jpeg_segments(d):
        if m == 0xE1:
            p = _payload(seg)
            if p.startswith(b"Exif\x00\x00"):
                hits["APP1/Exif"] = True
                for tag, val in _tiff_ifd0(p[6:]).items():
                    hits[NAMES.get(tag, f"tag:0x{tag:04X}")] = val
            elif b"xmpmeta" in p[:200] or p.startswith(b"http://ns.adobe.com/xap"):
                hits["APP1/XMP"] = True
        elif m == 0xED:
            hits["APP13/IPTC"] = True
    return hits


def strip_jpeg(d):
    orientation = None
    for m, seg, _ in _jpeg_segments(d):
        if m == 0xE1 and _payload(seg).startswith(b"Exif\x00\x00"):
            o = _tiff_ifd0(_payload(seg)[6:]).get(ORIENTATION)
            if o and o != 1:
                orientation = o
    out = bytearray(b"\xff\xd8")
    if orientation:
        out += _minimal_exif(orientation)
    for m, seg, trailing in _jpeg_segments(d):
        if m == 0xD8:
            continue
        if m == 0xE1:                                     # Exif and XMP both go
            continue
        if m == 0xED:                                     # IPTC
            continue
        if m == 0xE2 and not _payload(seg).startswith(b"ICC_PROFILE"):
            continue                                      # MPF etc; keep ICC
        out += seg
        if trailing:
            break
    return bytes(out)


def _png_chunks(d):
    i = 8
    while i + 8 <= len(d):
        sz = struct.unpack_from(">I", d, i)[0]
        cid = d[i + 4:i + 8]
        yield cid, d[i:i + 12 + sz]
        i += 12 + sz
        if cid == b"IEND":
            return


def scan_png(d):
    hits = {}
    for cid, chunk in _png_chunks(d):
        if cid in (b"eXIf", b"iTXt", b"tEXt", b"zTXt"):
            hits[cid.decode("latin1")] = True
            if cid == b"eXIf":
                for tag, val in _tiff_ifd0(chunk[8:-4]).items():
                    hits[NAMES.get(tag, f"tag:0x{tag:04X}")] = val
    return hits


def strip_png(d):
    return d[:8] + b"".join(c for cid, c in _png_chunks(d)
                            if cid not in (b"eXIf", b"iTXt", b"tEXt", b"zTXt"))


def scan_webp(d):
    hits = {}
    if d[:4] != b"RIFF" or d[8:12] != b"WEBP":
        return hits
    i = 12
    while i + 8 <= len(d):
        cid = d[i:i + 4]
        sz = struct.unpack_from("<I", d, i + 4)[0]
        if cid == b"EXIF":
            hits["EXIF chunk"] = True
            for tag, val in _tiff_ifd0(d[i + 8:i + 8 + sz]).items():
                hits[NAMES.get(tag, f"tag:0x{tag:04X}")] = val
        elif cid == b"XMP ":
            hits["XMP chunk"] = True
        i += 8 + sz + (sz & 1)
    return hits


HANDLERS = {
    ".jpg": (scan_jpeg, strip_jpeg), ".jpeg": (scan_jpeg, strip_jpeg),
    ".png": (scan_png, strip_png), ".webp": (scan_webp, None),
}


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    include_raw = "--include-raw" in sys.argv
    if len(args) != 2 or args[0] not in ("scan", "strip"):
        print(__doc__)
        return 2
    mode, root = args[0], pathlib.Path(args[1])
    targets = sorted(root.rglob("*")) if root.is_dir() else [root]
    gps = changed = skipped = 0
    for p in targets:
        if not p.is_file():
            continue
        if p.name.startswith("IMG_") and not include_raw:
            skipped += 1
            continue
        handler = HANDLERS.get(p.suffix.lower())
        if not handler:
            continue
        scan, strip = handler
        d = p.read_bytes()
        hits = scan(d)
        if mode == "strip" and strip and hits:
            new = strip(d)
            if scan(new).get("GPSInfo") is not None:
                print(f"ERROR {p}: GPS survived stripping")
                return 1
            p.write_bytes(new)
            changed += 1
            print(f"strip {p}  {len(d):>8} -> {len(new):>8} bytes  "
                  f"[dropped: {','.join(k for k in hits if not k.startswith('tag:'))}]")
            continue
        if "GPSInfo" in hits:
            gps += 1
        flag = "GPS!!" if "GPSInfo" in hits else ("meta " if hits else "clean")
        detail = ",".join(f"{k}" for k in sorted(hits)) or "-"
        print(f"{flag} {str(p):58s} {detail}")
    note = f" ({skipped} camera-named original(s) skipped)" if skipped else ""
    if mode == "strip":
        print(f"\nstripped {changed} file(s){note}")
        return 0
    print(f"\n{'FAIL' if gps else 'PASS'}: {gps} file(s) carrying GPS{note}")
    return 1 if gps else 0


if __name__ == "__main__":
    sys.exit(main())
