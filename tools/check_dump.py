#!/usr/bin/env python3
"""Check a dump written by examples/stress.rs with decoders that share no code
with the crate: Python's zlib module, and with --gzip N also the gzip program
on N of the outputs.

    python3 tools/check_dump.py dump.bin [--gzip 2000]

Each record: [u32 input length][u32 CRC-32 of the input][u32 output length][output].
"""
import struct, subprocess, sys, zlib

path = sys.argv[1]
n_gzip = int(sys.argv[sys.argv.index("--gzip") + 1]) if "--gzip" in sys.argv else 0
data = open(path, "rb").read()
off = count = total_in = total_out = 0
records = []
while off < len(data):
    in_len, crc, out_len = struct.unpack_from("<III", data, off)
    out = data[off + 12 : off + 12 + out_len]
    off += 12 + out_len
    d = zlib.decompressobj(31)
    back = d.decompress(out)
    assert d.eof and not d.unused_data, f"record {count}: trailing or missing data"
    assert len(back) == in_len and zlib.crc32(back) == crc, f"record {count}: wrong contents"
    records.append((in_len, crc, out))
    count += 1
    total_in += in_len
    total_out += out_len
print(f"{path}: {count} outputs read back by {zlib.ZLIB_RUNTIME_VERSION if hasattr(zlib, 'ZLIB_RUNTIME_VERSION') else 'zlib'}"
      f" ({total_in} -> {total_out} bytes)")
if n_gzip:
    step = max(1, count // n_gzip)
    done = 0
    for in_len, crc, out in records[::step]:
        r = subprocess.run(["gzip", "-dc"], input=out, capture_output=True)
        assert r.returncode == 0 and not r.stderr, r.stderr
        assert len(r.stdout) == in_len and zlib.crc32(r.stdout) == crc
        done += 1
    print(f"{path}: {done} of them also by {subprocess.run(['gzip', '--version'], capture_output=True, text=True).stdout.splitlines()[0]}")
