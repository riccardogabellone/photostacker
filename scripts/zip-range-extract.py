"""Extract one entry of a remote zip with HTTP Range requests only.

Usage: python3 -I scripts/zip-range-extract.py <zip-url> <entry-name> <output-file> [user-agent]

Reads the end-of-central-directory record from the last 128 KiB, the central
directory, then only the entry's local header and compressed bytes; inflates
them (stored or deflate), checks size and CRC32, and writes <output-file>.
Called by scripts/fetch-raw-samples.sh for the Empa (Zenodo) entries, whose
zips are 80-210 MB each. Standard library only; no ZIP64.
"""

import struct
import sys
import urllib.request
import zlib

TAIL = 128 * 1024


def fetch(url: str, ua: str, first: int | None = None, last: int | None = None, suffix: int | None = None):
    """The bytes of a Range request, and the total size from Content-Range."""
    rng = f"bytes=-{suffix}" if suffix is not None else f"bytes={first}-{last}"
    req = urllib.request.Request(url, headers={"User-Agent": ua, "Range": rng})
    with urllib.request.urlopen(req, timeout=600) as resp:
        if resp.status != 206:
            raise SystemExit(f"{url}: the server ignored the Range header (HTTP {resp.status})")
        total = int(resp.headers["Content-Range"].rsplit("/", 1)[1])
        return resp.read(), total


def central_directory(url: str, ua: str) -> dict[str, tuple[int, int, int, int, int]]:
    """Entry name -> (method, crc32, compressed size, size, local header offset)."""
    tail, total = fetch(url, ua, suffix=TAIL)
    eocd = tail.rfind(b"PK\x05\x06")
    if eocd < 0:
        raise SystemExit(f"{url}: no end-of-central-directory record in the last {TAIL} bytes")
    cd_size, cd_offset = struct.unpack("<II", tail[eocd + 12 : eocd + 20])
    tail_start = total - len(tail)
    if cd_offset >= tail_start:
        cd = tail[cd_offset - tail_start : cd_offset - tail_start + cd_size]
    else:
        cd, _ = fetch(url, ua, cd_offset, cd_offset + cd_size - 1)
    entries = {}
    pos = 0
    while cd[pos : pos + 4] == b"PK\x01\x02":
        (method,) = struct.unpack("<H", cd[pos + 10 : pos + 12])
        crc, csize, size = struct.unpack("<III", cd[pos + 16 : pos + 28])
        name_len, extra_len, comment_len = struct.unpack("<HHH", cd[pos + 28 : pos + 34])
        (offset,) = struct.unpack("<I", cd[pos + 42 : pos + 46])
        name = cd[pos + 46 : pos + 46 + name_len].decode("utf-8", "replace")
        entries[name] = (method, crc, csize, size, offset)
        pos += 46 + name_len + extra_len + comment_len
    return entries


def main() -> None:
    if len(sys.argv) not in (4, 5):
        raise SystemExit(__doc__)
    url, entry, out = sys.argv[1:4]
    ua = sys.argv[4] if len(sys.argv) == 5 else "photostacker-fixtures"
    entries = central_directory(url, ua)
    if entry not in entries:
        raise SystemExit(f"{url}: no entry {entry!r}")
    method, crc, csize, size, offset = entries[entry]
    header, _ = fetch(url, ua, offset, offset + 29)
    if header[:4] != b"PK\x03\x04":
        raise SystemExit(f"{url}: no local header for {entry!r} at {offset}")
    name_len, extra_len = struct.unpack("<HH", header[26:30])
    start = offset + 30 + name_len + extra_len
    data, _ = fetch(url, ua, start, start + csize - 1)
    if method == 8:
        data = zlib.decompress(data, -15)
    elif method != 0:
        raise SystemExit(f"{entry}: unsupported compression method {method}")
    if len(data) != size or zlib.crc32(data) & 0xFFFFFFFF != crc:
        raise SystemExit(f"{entry}: size or CRC32 mismatch")
    with open(out, "wb") as f:
        f.write(data)


if __name__ == "__main__":
    main()
