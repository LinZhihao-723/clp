#!/usr/bin/env python3
"""Writes `fixture_counts.tsv`: what the C++ reader reads from every fixture archive.

An independent reading of each single-file archive in `tests/fixtures/archives`, following the C++
reader (`ArchiveReaderAdaptor.cpp`, `ReaderUtils.cpp`, `ArchiveReader.cpp`,
`PackedStreamReader.cpp`, `DictionaryReader.hpp`, `TimestampDictionaryReader.cpp`). Frames are
decoded with the `zstd` CLI and msgpack with the `msgpack` package. Record counts and header sizes
and the authoritative timestamp ranges are cross-checked against the C++-generated `manifest.toml`
and `index.toml` (`clp-s` archive stats).

Usage: gen_fixture_counts.py PROTOTYPE_DIR (run with a Python that has `msgpack` and `tomli`).
"""

import struct
import subprocess
import sys
from pathlib import Path

import msgpack
import tomli

COLUMNS = [
    "file",
    "version",
    "num_nodes",
    "num_schemas",
    "num_streams",
    "num_tables",
    "num_log_events",
    "decoded_stream_size",
    "num_variables",
    "num_logtypes",
    "num_arrays",
    "num_timestamp_entries",
    "num_timestamp_patterns",
    "timestamp_begin_ms",
    "timestamp_end_ms",
    "num_range_entries",
    "has_log_order",
]


def zstd_decode(frame: bytes) -> bytes:
    return subprocess.run(
        ["zstd", "-d", "-c", "-q"], input=frame, capture_output=True, check=True
    ).stdout


class Reader:
    def __init__(self, data: bytes):
        self.data = data
        self.pos = 0

    def take(self, size: int) -> bytes:
        assert self.pos + size <= len(self.data), "truncated"
        out = self.data[self.pos : self.pos + size]
        self.pos += size
        return out

    def unpack(self, fmt: str):
        size = struct.calcsize(fmt)
        return struct.unpack("<" + fmt, self.take(size))[0]


def read_archive(path: Path) -> dict:
    data = path.read_bytes()
    header = Reader(data[:64])
    assert header.take(4) == bytes([0xFD, 0x2F, 0xC5, 0x30])
    version = header.unpack("I")
    uncompressed_size = header.unpack("Q")
    compressed_size = header.unpack("Q")
    header.take(32)
    metadata_size = header.unpack("I")
    assert header.unpack("H") == 0
    files_offset = 64 + metadata_size

    metadata = Reader(zstd_decode(data[64:files_offset]))
    sections = {}
    ts_entries = []
    ts_patterns = 0
    range_entries = 0
    for _ in range(metadata.unpack("B")):
        packet_type = metadata.unpack("B")
        payload = metadata.take(metadata.unpack("I"))
        if packet_type == 0:
            assert msgpack.unpackb(payload)["num_segments"] == 1
        elif packet_type == 1:
            files = next(iter(msgpack.unpackb(payload).values()))
            for index, entry in enumerate(files):
                begin = files_offset + entry["o"]
                end = (
                    files_offset + files[index + 1]["o"]
                    if index + 1 < len(files)
                    else compressed_size
                )
                sections[entry["n"]] = data[begin:end]
        elif packet_type == 2:
            ts = Reader(payload)
            for _ in range(ts.unpack("Q")):
                key = ts.take(ts.unpack("Q"))
                ids = [ts.unpack("i") for _ in range(ts.unpack("Q"))]
                encoding = ts.unpack("Q")
                bounds = None
                if encoding == 1:
                    bounds = (ts.unpack("q"), ts.unpack("q"))
                elif encoding == 2:
                    ts.take(16)
                ts_entries.append((key, ids, bounds))
            ts_patterns = ts.unpack("Q")
            for _ in range(ts_patterns):
                ts.unpack("Q")
                ts.take(ts.unpack("Q"))
        elif packet_type == 3:
            range_entries = len(msgpack.unpackb(payload))

    tree = Reader(zstd_decode(sections["/schema_tree"]))
    nodes = []
    for _ in range(tree.unpack("Q")):
        parent = tree.unpack("i")
        key = tree.take(tree.unpack("Q"))
        nodes.append((parent, key, tree.unpack("B")))
    metadata_roots = [
        index for index, (parent, key, kind) in enumerate(nodes) if parent == -1 and key == b"" and kind == 10
    ]
    has_log_order = any(
        parent in metadata_roots and key == b"log_event_idx" for parent, key, _ in nodes
    )

    schema_map = Reader(zstd_decode(sections["/schema_ids"]))
    num_schemas = schema_map.unpack("Q")
    for _ in range(num_schemas):
        schema_map.unpack("i")
        size = schema_map.unpack("I")
        schema_map.unpack("I")
        schema_map.take(4 * size)

    table_metadata = Reader(zstd_decode(sections["/table_metadata"]))
    num_streams = table_metadata.unpack("Q")
    stream_sizes = []
    for _ in range(num_streams):
        table_metadata.unpack("Q")
        stream_sizes.append(table_metadata.unpack("Q"))
    assert table_metadata.unpack("Q") == 0
    num_tables = table_metadata.unpack("Q")
    num_log_events = 0
    for _ in range(num_tables):
        table_metadata.unpack("Q")
        table_metadata.unpack("Q")
        table_metadata.unpack("i")
        num_log_events += table_metadata.unpack("Q")

    def dictionary_size(name: str) -> int:
        return struct.unpack("<Q", sections[name][:8])[0]

    authoritative = ts_entries[0][2] if ts_entries and ts_entries[0][2] else None
    return {
        "version": f"{version >> 24}.{(version >> 16) & 0xFF}.{version & 0xFFFF}",
        "num_nodes": len(nodes),
        "num_schemas": num_schemas,
        "num_streams": num_streams,
        "num_tables": num_tables,
        "num_log_events": num_log_events,
        "decoded_stream_size": sum(stream_sizes),
        "num_variables": dictionary_size("/var.dict"),
        "num_logtypes": dictionary_size("/log.dict"),
        "num_arrays": dictionary_size("/array.dict"),
        "num_timestamp_entries": len(ts_entries),
        "num_timestamp_patterns": ts_patterns,
        "timestamp_begin_ms": authoritative[0] if authoritative else "-",
        "timestamp_end_ms": authoritative[1] if authoritative else "-",
        "num_range_entries": range_entries,
        "has_log_order": int(has_log_order),
        "_uncompressed_size": uncompressed_size,
        "_compressed_size": compressed_size,
    }


def main() -> None:
    prototype_dir = Path(sys.argv[1])
    fixtures_dir = prototype_dir / "tests" / "fixtures"
    manifest = tomli.loads((fixtures_dir / "manifest.toml").read_text())
    index = tomli.loads((fixtures_dir / "archives" / "index.toml").read_text())
    sizes = {}
    for archive in index["archive"]:
        for file, size, uncompressed, begin, end in zip(
            archive["files"],
            archive["sizes"],
            archive["uncompressed_sizes"],
            archive["begin_timestamps"],
            archive["end_timestamps"],
        ):
            sizes[file] = (size, uncompressed, begin, end)

    rows = []
    for archive in manifest["archive"]:
        if archive["expect"] != "readable":
            continue
        counts = read_archive(fixtures_dir / archive["file"])
        assert counts["num_log_events"] == archive["records"], archive["file"]
        size, uncompressed, begin, end = sizes[archive["file"]]
        assert (counts["_compressed_size"], counts["_uncompressed_size"]) == (size, uncompressed)
        if counts["timestamp_begin_ms"] != "-":
            assert (counts["timestamp_begin_ms"], counts["timestamp_end_ms"]) == (begin, end)
        rows.append([Path(archive["file"]).name] + [str(counts[column]) for column in COLUMNS[1:]])

    output = prototype_dir / "crates" / "clp-s-format" / "tests" / "data" / "fixture_counts.tsv"
    with output.open("w") as out:
        out.write("# GENERATED by gen_fixture_counts.py; do not edit.\n")
        out.write("\t".join(COLUMNS) + "\n")
        for row in sorted(rows):
            out.write("\t".join(row) + "\n")


if __name__ == "__main__":
    main()
