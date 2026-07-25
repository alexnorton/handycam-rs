#!/usr/bin/env python3
"""Measure Sony video timestamps against the standard USB-audio stream.

The extractor metadata retains each isochronous descriptor's USB frame number.
This tool reconstructs an approximate monotonically increasing USB-frame
timeline from the batched URB completion time, finds Sony record headers on
endpoint 0x82, and reports audio continuity and video timestamp behavior.
"""

from __future__ import annotations

import argparse
import collections
import json
import statistics
import sys
from pathlib import Path
from typing import Any

USB_FRAME_MODULUS = 2048


def load_rows(path: Path) -> list[dict[str, Any]]:
    with path.open() as source:
        return [json.loads(line) for line in source if line.strip()]


def assign_unwrapped_usb_frames(rows: list[dict[str, Any]]) -> None:
    if not rows:
        return
    anchor = min(rows, key=lambda row: float(row["completion_time_epoch"]))
    anchor_time = float(anchor["completion_time_epoch"])
    anchor_frame = int(anchor["usb_frame"])
    for row in rows:
        completion_time = float(row["completion_time_epoch"])
        expected = anchor_frame + round((completion_time - anchor_time) * 1_000)
        frame = int(row["usb_frame"])
        wraps = round((expected - frame) / USB_FRAME_MODULUS)
        row["usb_frame_unwrapped"] = frame + wraps * USB_FRAME_MODULUS


def histogram(values: list[int], limit: int | None = None) -> dict[str, int]:
    counts = collections.Counter(values)
    if limit is None:
        items = sorted(counts.items())
    else:
        items = sorted(counts.items(), key=lambda item: (-item[1], item[0]))[:limit]
    return {str(value): count for value, count in items}


def continuity(rows: list[dict[str, Any]]) -> dict[str, Any]:
    frames = sorted({int(row["usb_frame_unwrapped"]) for row in rows})
    gaps = [
        current - previous - 1
        for previous, current in zip(frames, frames[1:], strict=False)
        if current > previous + 1
    ]
    duplicates = len(rows) - len(frames)
    return {
        "descriptors": len(rows),
        "unique_usb_frames": len(frames),
        "duplicate_usb_frames": duplicates,
        "first_usb_frame": frames[0] if frames else None,
        "last_usb_frame": frames[-1] if frames else None,
        "missing_frame_runs": len(gaps),
        "missing_frames": sum(gaps),
        "largest_missing_run": max(gaps, default=0),
        "most_common_packet_lengths": histogram(
            [int(row["length"]) for row in rows], limit=12
        ),
    }


def video_headers(
    rows: list[dict[str, Any]], video_data: bytes
) -> list[dict[str, Any]]:
    headers = []
    for row in rows:
        if row["endpoint"] != "0x82" or int(row["length"]) < 8:
            continue
        offset = int(row["data_offset"])
        header = video_data[offset : offset + 8]
        if header[:6] != b"\xff" * 6:
            continue
        timestamp = ((header[6] & 0x07) << 8) | header[7]
        headers.append(
            {
                "usb_frame": int(row["usb_frame_unwrapped"]),
                "usb_frame_modulo": int(row["usb_frame"]),
                "timestamp": timestamp,
                "flags": header[6] & 0xF8,
                "completion_time_epoch": float(row["completion_time_epoch"]),
                "capture_frame": int(row["capture_frame"]),
            }
        )
    return headers


def signed_modulo_difference(left: int, right: int) -> int:
    return (left - right + USB_FRAME_MODULUS // 2) % USB_FRAME_MODULUS - (
        USB_FRAME_MODULUS // 2
    )


def analyze_video(headers: list[dict[str, Any]]) -> dict[str, Any]:
    deltas = [
        (current["timestamp"] - previous["timestamp"]) % USB_FRAME_MODULUS
        for previous, current in zip(headers, headers[1:], strict=False)
    ]
    phase = [
        signed_modulo_difference(header["usb_frame_modulo"], header["timestamp"])
        for header in headers
    ]
    zero_run = 0
    maximum_zero_run = 0
    longest_zero_run_seconds = 0.0
    for delta in deltas:
        if delta == 0:
            zero_run += 1
            maximum_zero_run = max(maximum_zero_run, zero_run)
        else:
            zero_run = 0
    # Compute the duration separately without relying on duplicate delta
    # values or the loop's index bookkeeping.
    run_start = None
    for index, delta in enumerate(deltas):
        if delta == 0:
            if run_start is None:
                run_start = index
            duration = (
                headers[index + 1]["completion_time_epoch"]
                - headers[run_start]["completion_time_epoch"]
            )
            longest_zero_run_seconds = max(longest_zero_run_seconds, duration)
        else:
            run_start = None
    phase_counts = collections.Counter(phase)
    return {
        "headers": len(headers),
        "timestamp_delta_histogram": histogram(deltas),
        "longest_repeated_timestamp_run_after_first": maximum_zero_run,
        "longest_repeated_timestamp_run_seconds": round(longest_zero_run_seconds, 6),
        "usb_frame_minus_timestamp_modulo_2048": {
            "minimum": min(phase, default=None),
            "median": statistics.median(phase) if phase else None,
            "maximum": max(phase, default=None),
            "within_8ms": sum(abs(value) <= 8 for value in phase),
            "most_common": {
                str(value): count for value, count in phase_counts.most_common(10)
            },
        },
        "first_header": headers[0] if headers else None,
        "last_header": headers[-1] if headers else None,
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "capture",
        nargs="?",
        type=Path,
        default=Path("reverse-engineering/captures/extracted-record-mode-ohci-all"),
        help="directory containing iso-packets.jsonl and endpoint binaries",
    )
    return parser.parse_args()


def main() -> int:
    arguments = parse_args()
    try:
        rows = load_rows(arguments.capture / "iso-packets.jsonl")
        assign_unwrapped_usb_frames(rows)
        video_data = (arguments.capture / "ep82.bin").read_bytes()
    except (OSError, ValueError, KeyError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    audio_rows = [row for row in rows if row["endpoint"] == "0x83"]
    boundary_rows = [row for row in rows if row["endpoint"] == "0x81"]
    compressed_rows = [row for row in rows if row["endpoint"] == "0x82"]
    headers = video_headers(rows, video_data)
    result = {
        "schema": 1,
        "capture": str(arguments.capture),
        "audio": continuity(audio_rows),
        "boundary_endpoint": continuity(boundary_rows),
        "video_endpoint": continuity(compressed_rows),
        "video_records": analyze_video(headers),
        "derived_format": {
            "audio_sample_rate_hz": 16_000,
            "audio_channels": 2,
            "audio_bytes_per_sample_frame": 4,
            "audio_sample_frames_per_usb_frame": 16,
            "audio_sample_frames_per_40ms_video_frame": 640,
        },
    }
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
