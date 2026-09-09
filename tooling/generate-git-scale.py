#!/usr/bin/env python3
"""Generate a sparse, valid Git pack and worktree for the ignored Git scale suite."""

import argparse
import hashlib
import os
from pathlib import Path
import struct
import subprocess
import tempfile
import zlib


def git(root, *args, input=None):
    return subprocess.check_output(["git", "-C", str(root), *args], input=input).strip()


def generate(root, object_count, pack_bytes, blob_bytes, tracked_bytes, branches):
    root.mkdir(parents=True, exist_ok=False)
    git(root, "init", "--initial-branch=main")
    administration = root / ".git"
    packs = administration / "objects" / "pack"
    temporary = packs / "generated.pack"
    zeros = bytes(65535)
    records_path = root.parent / (root.name + "-index-records")
    fanout = [0] * 256
    large_sizes = [blob_bytes, tracked_bytes - blob_bytes]
    remaining = pack_bytes - sum(large_sizes)
    while remaining:
        size = min(blob_bytes, remaining)
        large_sizes.append(size)
        remaining -= size
    if object_count < len(large_sizes):
        raise ValueError("object count cannot hold the requested pack content")
    selected = []
    pack_hash = hashlib.sha1()
    with temporary.open("wb") as output, records_path.open("w") as records:
        def write(data):
            output.write(data)
            pack_hash.update(data)

        write(b"PACK" + struct.pack(">II", 2, object_count))
        for sequence in range(object_count):
            size = large_sizes[sequence] if sequence < len(large_sizes) else 16
            suffix = f"{sequence:016x}".encode()
            offset = output.tell()
            encoded_size = size >> 4
            header = bytearray([(3 << 4) | (size & 15) | (128 if encoded_size else 0)])
            while encoded_size:
                part = encoded_size & 127
                encoded_size >>= 7
                header.append(part | (128 if encoded_size else 0))
            write(header)
            crc = zlib.crc32(header)
            content_hash = hashlib.sha1(f"blob {size}\0".encode())
            if size == 16:
                compressed = zlib.compress(suffix)
                write(compressed)
                crc = zlib.crc32(compressed, crc)
                content_hash.update(suffix)
            else:
                write(b"\x78\x01")
                crc = zlib.crc32(b"\x78\x01", crc)
                adler = 1
                remaining = size
                while remaining:
                    count = min(65535, remaining)
                    final = count == remaining
                    block_header = struct.pack("<BHH", int(final), count, count ^ 65535)
                    write(block_header)
                    crc = zlib.crc32(block_header, crc)
                    tail_count = min(16, count) if final else 0
                    # Leave at least the unique suffix for the final stored block.
                    if not final and remaining - count < 16:
                        raise ValueError("choose blob sizes with a final block of at least 16 bytes")
                    zero_count = count - tail_count
                    zero_chunk = zeros[:zero_count]
                    output.seek(zero_count, os.SEEK_CUR)
                    pack_hash.update(zero_chunk)
                    crc = zlib.crc32(zero_chunk, crc)
                    adler = zlib.adler32(zero_chunk, adler)
                    content_hash.update(zero_chunk)
                    if tail_count:
                        write(suffix)
                        crc = zlib.crc32(suffix, crc)
                        adler = zlib.adler32(suffix, adler)
                        content_hash.update(suffix)
                    remaining -= count
                trailer = struct.pack(">I", adler)
                write(trailer)
                crc = zlib.crc32(trailer, crc)
            oid = content_hash.hexdigest()
            fanout[int(oid[:2], 16)] += 1
            records.write(f"{oid} {crc:08x} {offset:016x}\n")
            if sequence < 2:
                selected.append((oid, size, suffix))
            if sequence and sequence % 500_000 == 0:
                print(f"generated {sequence} objects", flush=True)
        checksum = pack_hash.digest()
        output.write(checksum)
    pack_name = "pack-" + checksum.hex()
    temporary.rename(packs / (pack_name + ".pack"))
    sorted_records = records_path.with_suffix(".sorted")
    with sorted_records.open("wb") as output:
        subprocess.run(["sort", "-S", "8M", str(records_path)], stdout=output, env={**os.environ, "LC_ALL": "C"}, check=True)
    records_path.unlink()
    with (packs / (pack_name + ".idx")).open("wb") as index, tempfile.TemporaryFile() as large_offsets:
        index_hash = hashlib.sha1()
        def index_write(data):
            index.write(data)
            index_hash.update(data)
        index_write(b"\xfftOc" + struct.pack(">I", 2))
        count = 0
        for bucket in fanout:
            count += bucket
            index_write(struct.pack(">I", count))
        for field in [0, 1, 2]:
            large_count = 0
            with sorted_records.open() as records:
                for line in records:
                    parts = line.split()
                    if field == 0:
                        index_write(bytes.fromhex(parts[0]))
                    elif field == 1:
                        index_write(bytes.fromhex(parts[1]))
                    else:
                        offset = int(parts[2], 16)
                        if offset < 0x80000000:
                            index_write(struct.pack(">I", offset))
                        else:
                            index_write(struct.pack(">I", 0x80000000 | large_count))
                            large_offsets.write(struct.pack(">Q", offset))
                            large_count += 1
        large_offsets.seek(0)
        while chunk := large_offsets.read(65536):
            index_write(chunk)
        index_write(checksum)
        index.write(index_hash.digest())
    sorted_records.unlink()
    tree = bytearray()
    for name, (oid, size, suffix) in zip(["large.bin", "second.bin"], selected):
        tree.extend(b"100644 " + name.encode() + b"\0" + bytes.fromhex(oid))
        with (root / name).open("wb") as output:
            output.seek(size - len(suffix))
            output.write(suffix)
    tree_oid = git(root, "hash-object", "-t", "tree", "-w", "--stdin", input=tree)
    commit = b"tree " + tree_oid + b"\nauthor Scale Fixture <scale@example.test> 0 +0000\ncommitter Scale Fixture <scale@example.test> 0 +0000\n\nScale fixture\n"
    commit_oid = git(root, "hash-object", "-t", "commit", "-w", "--stdin", input=commit)
    git(root, "update-ref", "refs/heads/main", commit_oid.decode())
    git(root, "read-tree", "HEAD")
    updates = b"".join(f"create refs/heads/scale-{branch} {commit_oid.decode()}\n".encode() for branch in range(branches - 1))
    git(root, "update-ref", "--stdin", input=updates)
    print(f"objects={object_count} pack_bytes={(packs / (pack_name + '.pack')).stat().st_size} tracked_bytes={tracked_bytes} largest_blob={blob_bytes} branches={branches}", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", type=Path)
    parser.add_argument("--objects", type=int, default=3_700_000)
    parser.add_argument("--pack-bytes", type=int, default=46 * 1024**3)
    parser.add_argument("--blob-bytes", type=int, default=1_000_000_000)
    parser.add_argument("--tracked-bytes", type=int, default=1_700_000_000)
    parser.add_argument("--branches", type=int, default=5_000)
    args = parser.parse_args()
    generate(args.root, args.objects, args.pack_bytes, args.blob_bytes, args.tracked_bytes, args.branches)
