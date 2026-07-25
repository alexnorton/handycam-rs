#!/usr/bin/env python3
"""Wrap Sony DCR-HC24 entropy records as standard baseline JPEG images.

The camera sends an eight-byte Sony header followed by an unstuffed baseline
JPEG entropy stream. Its initialization matrices match the standard JPEG
quality-50 tables, and the stream uses 4:2:0 sampling and standard Huffman
tables. Pillow is used only to generate a canonical marker/table header and
to validate each resulting image.
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import sys
from pathlib import Path
from typing import Any

from PIL import Image


WIDTH = 320
HEIGHT = 240
JPEG_QUALITY = 50
PIL_SUBSAMPLING_420 = 2


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def jpeg_header() -> bytes:
    buffer = io.BytesIO()
    Image.new("RGB", (WIDTH, HEIGHT)).save(
        buffer,
        "JPEG",
        quality=JPEG_QUALITY,
        subsampling=PIL_SUBSAMPLING_420,
        optimize=False,
    )
    template = buffer.getvalue()
    sos = template.index(b"\xff\xda")
    segment_length = int.from_bytes(template[sos + 2 : sos + 4], "big")
    scan_start = sos + 2 + segment_length
    return template[:scan_start]


def wrap_record(record: bytes, header: bytes) -> bytes:
    if len(record) < 9 or record[:6] != b"\xff" * 6:
        raise ValueError("record does not contain an eight-byte Sony header")
    # Complete records end with zero alignment padding. Removing it before
    # adding JPEG byte stuffing avoids feeding padding as extra MCUs.
    entropy = record[8:].rstrip(b"\x00")
    if not entropy:
        raise ValueError("record has no entropy payload")
    stuffed_entropy = entropy.replace(b"\xff", b"\xff\x00")
    return header + stuffed_entropy + b"\xff\xd9"


def decode(records_directory: Path, output: Path) -> dict[str, Any]:
    manifest_path = records_directory / "records.json"
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    header = jpeg_header()
    output.mkdir(parents=True, exist_ok=False)
    images = []
    failures = []

    for item in manifest["records"]:
        if not item["bounded_by_next_header"]:
            continue
        source = records_directory / item["file"]
        destination_name = f"frame-{int(item['index']):04d}.jpg"
        destination = output / destination_name
        try:
            jpeg = wrap_record(source.read_bytes(), header)
            with Image.open(io.BytesIO(jpeg)) as image:
                image.load()
                if image.size != (WIDTH, HEIGHT):
                    raise ValueError(
                        f"decoded dimensions are {image.size}, "
                        f"expected {(WIDTH, HEIGHT)}"
                    )
            destination.write_bytes(jpeg)
            images.append(
                {
                    "record_index": item["index"],
                    "file": destination_name,
                    "bytes": len(jpeg),
                    "sha256": sha256(jpeg),
                    "timestamp_ms": item["timestamp_ms"],
                    "timestamp_delta_to_next_ms": item[
                        "timestamp_delta_to_next_ms"
                    ],
                }
            )
        except (OSError, ValueError) as error:
            failures.append(
                {
                    "record_index": item["index"],
                    "source": item["file"],
                    "error": str(error),
                }
            )

    summary = {
        "source_records": str(records_directory),
        "width": WIDTH,
        "height": HEIGHT,
        "jpeg_quality_tables": JPEG_QUALITY,
        "jpeg_subsampling": "4:2:0",
        "decoded_count": len(images),
        "failure_count": len(failures),
        "images": images,
        "failures": failures,
    }
    with (output / "frames.json").open("w", encoding="utf-8") as stream:
        json.dump(summary, stream, indent=2)
        stream.write("\n")
    return summary


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("records_directory", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()

    records_directory = args.records_directory.resolve()
    output = args.output.resolve()
    if not records_directory.is_dir():
        parser.error(
            f"records directory does not exist: {records_directory}"
        )
    if output.exists():
        parser.error(f"output already exists: {output}")
    summary = decode(records_directory, output)
    print(
        json.dumps(
            {
                key: value
                for key, value in summary.items()
                if key not in ("images", "failures")
            },
            indent=2,
        )
    )
    if summary["failures"]:
        for failure in summary["failures"]:
            print(
                f"record {failure['record_index']}: {failure['error']}",
                file=sys.stderr,
            )
        return 1
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1)
