#!/usr/bin/env python3
"""Split endpoint 0x82 data into timestamped Sony compressed-video records.

Input is either a live handycam-capture directory or an
extract_sony_stream.py directory. Headers are accepted only at the start of a
recorded USB isochronous packet, matching sonypvs1.sys's framing rule.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import statistics
import sys
from pathlib import Path
from typing import Any


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def load_header_offsets(
    directory: Path, ep82: bytes
) -> list[dict[str, int]]:
    metadata = directory / "iso-packets.jsonl"
    headers = []
    with metadata.open(encoding="utf-8") as stream:
        for line_number, line in enumerate(stream, 1):
            record: dict[str, Any] = json.loads(line)
            if record.get("endpoint") != "0x82":
                continue
            offset = record.get("data_offset")
            length = int(record.get("length", 0))
            if offset is None or length < 8:
                continue
            offset = int(offset)
            if offset < 0 or offset + length > len(ep82):
                raise ValueError(
                    f"metadata line {line_number}: endpoint data range "
                    f"{offset}+{length} exceeds ep82.bin ({len(ep82)} bytes)"
                )
            if ep82[offset : offset + 6] != b"\xff" * 6:
                continue
            timestamp = ((ep82[offset + 6] & 0x07) << 8) | ep82[offset + 7]
            headers.append(
                {
                    "offset": offset,
                    "packet_length": length,
                    "timestamp_ms": timestamp,
                    "packet_sequence": int(
                        record.get(
                            "packet_sequence",
                            record.get("capture_frame", line_number),
                        )
                    ),
                    "usb_frame": int(record["usb_frame"])
                    if "usb_frame" in record
                    else -1,
                }
            )
    headers.sort(key=lambda item: item["offset"])
    if not headers:
        raise ValueError("no descriptor-start Sony headers found")
    if len({item["offset"] for item in headers}) != len(headers):
        raise ValueError("metadata contains duplicate Sony header offsets")
    return headers


def split(directory: Path, output: Path) -> dict[str, Any]:
    ep82_path = directory / "ep82.bin"
    ep82 = ep82_path.read_bytes()
    headers = load_header_offsets(directory, ep82)
    output.mkdir(parents=True, exist_ok=False)
    records = []

    for index, header in enumerate(headers):
        start = header["offset"]
        end = (
            headers[index + 1]["offset"]
            if index + 1 < len(headers)
            else len(ep82)
        )
        data = ep82[start:end]
        filename = f"record-{index + 1:04d}.bin"
        (output / filename).write_bytes(data)
        next_timestamp = (
            headers[index + 1]["timestamp_ms"]
            if index + 1 < len(headers)
            else None
        )
        records.append(
            {
                "index": index + 1,
                "file": filename,
                "data_offset": start,
                "length": len(data),
                "payload_length_after_8_byte_header": max(0, len(data) - 8),
                "sha256": sha256(data),
                "timestamp_ms": header["timestamp_ms"],
                "timestamp_delta_to_next_ms": (
                    (next_timestamp - header["timestamp_ms"]) % 2048
                    if next_timestamp is not None
                    else None
                ),
                "source_packet_length": header["packet_length"],
                "source_packet_sequence": header["packet_sequence"],
                "source_usb_frame": (
                    header["usb_frame"]
                    if header["usb_frame"] >= 0
                    else None
                ),
                "bounded_by_next_header": index + 1 < len(headers),
            }
        )

    complete_lengths = [
        item["length"] for item in records if item["bounded_by_next_header"]
    ]
    normal_records = [
        item
        for item in records
        if item["bounded_by_next_header"]
        and item["timestamp_delta_to_next_ms"] == 40
    ]
    summary = {
        "source_directory": str(directory),
        "source_ep82": str(ep82_path),
        "source_ep82_sha256": sha256(ep82),
        "source_ep82_bytes": len(ep82),
        "record_count": len(records),
        "complete_record_count": len(complete_lengths),
        "normal_40ms_record_count": len(normal_records),
        "complete_record_length": {
            "minimum": min(complete_lengths, default=None),
            "median": statistics.median(complete_lengths)
            if complete_lengths
            else None,
            "maximum": max(complete_lengths, default=None),
        },
        "records": records,
    }
    with (output / "records.json").open("w", encoding="utf-8") as stream:
        json.dump(summary, stream, indent=2)
        stream.write("\n")
    return summary


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()

    directory = args.directory.resolve()
    output = args.output.resolve()
    if not directory.is_dir():
        parser.error(f"input directory does not exist: {directory}")
    if output.exists():
        parser.error(f"output already exists: {output}")
    summary = split(directory, output)
    display = {key: value for key, value in summary.items() if key != "records"}
    print(json.dumps(display, indent=2))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1)
