#!/usr/bin/env python3
"""Offline, checksum-verified cleanup of collection_gap metadata; dry-run by default.
Stop Haze before --apply. Preserves record IDs, sequence high-water marks, paths,
loss events and checkpoints. Requires the system libzstd, not Python packages.
"""
import argparse
import ctypes
import ctypes.util
import hashlib
import json
import os
from pathlib import Path
import struct

lib = ctypes.CDLL(ctypes.util.find_library('zstd') or 'libzstd.so.1')
for name in ('ZSTD_decompress', 'ZSTD_compress'):
    getattr(lib, name).restype = ctypes.c_size_t
lib.ZSTD_decompress.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_void_p, ctypes.c_size_t]
lib.ZSTD_compress.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int]
lib.ZSTD_isError.argtypes = [ctypes.c_size_t]
lib.ZSTD_isError.restype = ctypes.c_uint
LIMIT = 8 * 1024 * 1024

def codec(data, compress=False):
    out = ctypes.create_string_buffer(LIMIT + 65536)
    fn = lib.ZSTD_compress if compress else lib.ZSTD_decompress
    args = (out, len(out), data, len(data))
    n = fn(*args, 3) if compress else fn(*args)
    if lib.ZSTD_isError(n) or n > LIMIT:
        raise ValueError('Invalid or oversized zstd metadata')
    return ctypes.string_at(out, n)

def dump(obj):
    return json.dumps(obj, separators=(',', ':'), ensure_ascii=False).encode()

def digest(data):
    return hashlib.sha256(data).digest()

def decode_block(raw):
    assert raw[:4] == b'HZM1', 'Unsupported metadata format'
    n = struct.unpack('<I', raw[4:8])[0]
    header, body = raw[40:40+n], raw[72+n:]
    assert digest(header) == raw[8:40], 'Index checksum mismatch'
    assert digest(body) == raw[40+n:72+n], 'Body checksum mismatch'
    h, b = json.loads(codec(header)), json.loads(codec(body))
    assert h['version'] == b['version'] == 1
    assert {r['id'] for r in h['entries']} == {r['id'] for r in b['records']}
    return h, b

def encode_block(h, b):
    header, body = codec(dump(h), True), codec(dump(b), True)
    return b'HZM1' + struct.pack('<I', len(header)) + digest(header) + header + digest(body) + body

def decode_wal(raw):
    records = []; pos = 0
    while pos < len(raw):
        assert len(raw)-pos >= 36, 'Incomplete WAL; stop Haze cleanly first'
        n = struct.unpack('<I', raw[pos:pos+4])[0]
        payload = raw[pos+36:pos+36+n]
        assert len(payload) == n and digest(payload) == raw[pos+4:pos+36], 'WAL checksum mismatch'
        records.append((json.loads(payload), raw[pos:pos+36+n])); pos += 36+n
    return records

def atomic(path, data):
    mode = path.stat().st_mode & 0o777 if path.exists() else 0o600
    temp = path.with_name(path.name + '.cleanup-tmp')
    with temp.open('wb') as f:
        f.write(data); f.flush(); os.fsync(f.fileno())
    os.chmod(temp, mode)
    if path.exists():
        st = path.stat(); os.chown(temp, st.st_uid, st.st_gid)
    else:
        st = path.parent.stat(); os.chown(temp, st.st_uid, st.st_gid)
    os.replace(temp, path)

def clean(directory, before, apply):
    updates = []; removed = set(); retained = set(); high = 0
    def matches(r, ts):
        return r['kind'] == 'trace' and r['data'].get('event') == 'collection_gap' and ts <= before
    for path in sorted(directory.glob('*.hzm.zst')):
        h, b = decode_block(path.read_bytes()); high = max(high, h['last'] + 1)
        drop = {r['id'] for r in b['records'] if matches(r, h['start'] + r['timestamp_delta'])}
        removed.update(drop); retained.update(r['id'] for r in b['records'] if r['id'] not in drop)
        if not drop: continue
        b['records'] = [r for r in b['records'] if r['id'] not in drop]
        h['entries'] = [r for r in h['entries'] if r['id'] not in drop]
        # Preserve covered sequence/time bounds and filenames, so superseded
        # blocks cannot reappear and the WAL's committed boundary stays valid.
        updates.append((path, encode_block(h, b) if b['records'] else None))
    wal = directory / 'active.wal'
    if wal.exists():
        records = decode_wal(wal.read_bytes()); kept = []
        for r, raw in records:
            high = max(high, r['sequence'] + 1)
            if matches(r, r['timestamp']): removed.add(r['id'])
            else: kept.append(raw); retained.add(r['id'])
        if len(kept) != len(records): updates.append((wal, b''.join(kept)))
    if apply and updates:
        sequence = directory / 'sequence'
        if sequence.exists(): high = max(high, int(sequence.read_text()))
        atomic(sequence, str(high).encode())
        for path, data in updates:
            if data is None: path.unlink()
            else: atomic(path, data)
        fd = os.open(directory, os.O_RDONLY)
        try: os.fsync(fd)
        finally: os.close(fd)
    return len(removed), len(retained), len(updates)

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('root', type=Path)
    parser.add_argument('--before', type=int, required=True)
    parser.add_argument('--apply', action='store_true')
    args = parser.parse_args()
    total = changed = files = kept = 0
    for directory in sorted(args.root.glob('*/*/metadata')):
        n, k, f = clean(directory, args.before, args.apply)
        total += n; kept += k; files += f; changed += bool(n)
    print(json.dumps({'apply': args.apply, 'before': args.before, 'collection_gaps': total,
                      'hosts_affected': changed, 'files_changed': files, 'other_records_preserved': kept}))
if __name__ == '__main__': main()
