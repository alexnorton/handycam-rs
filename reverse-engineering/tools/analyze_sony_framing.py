#!/usr/bin/env python3
"""Compare Sony 054c:00c0 cross-endpoint framing from extracted usbmon data.

Input directories are produced by extract_sony_stream.py. The report models
the ordering-sensitive part of sonypvs1.sys: endpoint 0x81 byte 0 bit 3 sets
a boundary-ready flag, and a later endpoint 0x82 record beginning with at
least six 0xff bytes consumes it.
"""

from __future__ import annotations

import argparse
import json
from collections import Counter
from dataclasses import dataclass
from pathlib import Path
from typing import Any


@dataclass(frozen=True)
class Descriptor:
    capture_frame: int
    completion_time: float
    endpoint: str
    urb_id: str
    usb_frame: int
    length: int
    data_offset: int


@dataclass
class Transfer:
    capture_frame: int
    completion_time: float
    endpoint: str
    urb_id: str
    boundary_packets: int = 0
    headers: int = 0


def load_descriptors(path: Path) -> dict[str, list[Descriptor]]:
    descriptors: dict[str, list[Descriptor]] = {
        "0x81": [],
        "0x82": [],
        "0x83": [],
    }
    with path.open(encoding="utf-8") as stream:
        for line in stream:
            record: dict[str, Any] = json.loads(line)
            endpoint = record["endpoint"]
            if endpoint not in descriptors or not record["length"]:
                continue
            descriptors[endpoint].append(
                Descriptor(
                    capture_frame=record["capture_frame"],
                    completion_time=float(record["completion_time_epoch"]),
                    endpoint=endpoint,
                    urb_id=record["urb_id"],
                    usb_frame=record["usb_frame"],
                    length=record["length"],
                    data_offset=record["data_offset"],
                )
            )
    return descriptors


def ff_run_offsets(data: bytes, minimum: int = 6) -> list[int]:
    offsets = []
    position = 0
    while position < len(data):
        start = data.find(b"\xff" * minimum, position)
        if start < 0:
            break
        end = start + minimum
        while end < len(data) and data[end] == 0xFF:
            end += 1
        offsets.append(start)
        position = end
    return offsets


def timestamp_report(
    descriptors: list[Descriptor], data: bytes
) -> dict[str, Any]:
    payloads = [
        data[item.data_offset : item.data_offset + item.length]
        for item in descriptors
        if item.length >= 8
    ]
    timestamps = [
        ((payload[6] & 0x07) << 8) | payload[7]
        for payload in payloads
    ]
    deltas = [
        (current - previous) % 2048
        for previous, current in zip(timestamps, timestamps[1:])
    ]
    return {
        "timestamp_count": len(timestamps),
        "first_timestamp_ms": timestamps[0] if timestamps else None,
        "last_timestamp_ms": timestamps[-1] if timestamps else None,
        "consecutive_duplicate_timestamps": sum(
            previous == current
            for previous, current in zip(timestamps, timestamps[1:])
        ),
        "consecutive_identical_payloads": sum(
            previous == current
            for previous, current in zip(payloads, payloads[1:])
        ),
        "expected_40ms_steps": sum(delta == 40 for delta in deltas),
        "delta_ms_counts": dict(sorted(Counter(deltas).items())),
    }


def simulate_ordering(
    timeline: list[Transfer],
    boundaries_by_transfer: dict[tuple[int, str], list[Descriptor]],
    headers_by_transfer: dict[tuple[int, str], list[Descriptor]],
) -> dict[str, Any]:
    flag = False
    candidate_samples = 0
    overwritten_boundaries = 0
    headers_without_boundary = 0
    boundary_to_header_ms = []
    boundary_to_header_usb_frames = []
    candidate_sample_times = []
    last_boundary: Descriptor | None = None

    for transfer in timeline:
        key = (transfer.capture_frame, transfer.urb_id)
        if transfer.endpoint == "0x81" and transfer.boundary_packets:
            for boundary in boundaries_by_transfer[key]:
                if flag:
                    overwritten_boundaries += 1
                flag = True
                last_boundary = boundary
        elif transfer.endpoint == "0x82" and transfer.headers:
            for header in headers_by_transfer[key]:
                if flag and last_boundary is not None:
                    candidate_samples += 1
                    candidate_sample_times.append(header.completion_time)
                    boundary_to_header_ms.append(
                        (header.completion_time - last_boundary.completion_time)
                        * 1000
                    )
                    boundary_to_header_usb_frames.append(
                        (header.usb_frame - last_boundary.usb_frame) % 2048
                    )
                    flag = False
                    last_boundary = None
                else:
                    headers_without_boundary += 1

    def percentile(values: list[float], fraction: float) -> float | None:
        if not values:
            return None
        ordered = sorted(values)
        index = round((len(ordered) - 1) * fraction)
        return ordered[index]

    return {
        "candidate_samples": candidate_samples,
        "overwritten_boundaries": overwritten_boundaries,
        "headers_without_boundary": headers_without_boundary,
        "boundary_left_pending": flag,
        "completion_delay_ms": {
            "minimum": min(boundary_to_header_ms, default=None),
            "median": percentile(boundary_to_header_ms, 0.5),
            "p95": percentile(boundary_to_header_ms, 0.95),
            "maximum": max(boundary_to_header_ms, default=None),
        },
        "candidate_samples_per_second": dict(
            sorted(
                Counter(
                    int(completion_time - timeline[0].completion_time)
                    for completion_time in candidate_sample_times
                ).items()
            )
        )
        if timeline
        else {},
        "usb_frame_delta_counts": dict(
            sorted(Counter(boundary_to_header_usb_frames).items())
        ),
    }


def analyze(directory: Path, segment_gap_ms: float) -> dict[str, Any]:
    metadata = directory / "iso-packets.jsonl"
    ep81_data = (directory / "ep81.bin").read_bytes()
    ep82_data = (directory / "ep82.bin").read_bytes()
    descriptors = load_descriptors(metadata)

    transfers: dict[tuple[int, str], Transfer] = {}

    def transfer_for(descriptor: Descriptor) -> Transfer:
        key = (descriptor.capture_frame, descriptor.urb_id)
        if key not in transfers:
            transfers[key] = Transfer(
                capture_frame=descriptor.capture_frame,
                completion_time=descriptor.completion_time,
                endpoint=descriptor.endpoint,
                urb_id=descriptor.urb_id,
            )
        return transfers[key]

    boundary_descriptors = []
    for descriptor in descriptors["0x81"]:
        payload = ep81_data[
            descriptor.data_offset : descriptor.data_offset + descriptor.length
        ]
        if payload[0] & 0x08:
            transfer_for(descriptor).boundary_packets += 1
            boundary_descriptors.append(descriptor)

    all_ff_runs = ff_run_offsets(ep82_data)
    header_descriptors = []
    for descriptor in descriptors["0x82"]:
        offset = descriptor.data_offset
        if ep82_data[offset : offset + 6] == b"\xff" * 6:
            transfer_for(descriptor).headers += 1
            header_descriptors.append(descriptor)

    boundaries_by_transfer: dict[tuple[int, str], list[Descriptor]] = {}
    headers_by_transfer: dict[tuple[int, str], list[Descriptor]] = {}
    for descriptor in boundary_descriptors:
        boundaries_by_transfer.setdefault(
            (descriptor.capture_frame, descriptor.urb_id), []
        ).append(descriptor)
    for descriptor in header_descriptors:
        headers_by_transfer.setdefault(
            (descriptor.capture_frame, descriptor.urb_id), []
        ).append(descriptor)

    timeline = sorted(transfers.values(), key=lambda item: item.capture_frame)
    segment_gap = segment_gap_ms / 1000
    transfer_segments: list[list[Transfer]] = []
    for transfer in timeline:
        if (
            not transfer_segments
            or transfer.completion_time
            - transfer_segments[-1][-1].completion_time
            > segment_gap
        ):
            transfer_segments.append([transfer])
        else:
            transfer_segments[-1].append(transfer)

    first_completion = timeline[0].completion_time if timeline else 0.0
    segments = []
    for index, segment in enumerate(transfer_segments, start=1):
        segment_capture_frames = {
            transfer.capture_frame for transfer in segment
        }
        segment_boundaries = [
            descriptor
            for descriptor in boundary_descriptors
            if descriptor.capture_frame in segment_capture_frames
        ]
        segment_headers = [
            descriptor
            for descriptor in header_descriptors
            if descriptor.capture_frame in segment_capture_frames
        ]
        segments.append(
            {
                "index": index,
                "start_seconds": (
                    segment[0].completion_time - first_completion
                ),
                "duration_seconds": (
                    segment[-1].completion_time
                    - segment[0].completion_time
                ),
                "boundary_packets": sum(
                    transfer.boundary_packets for transfer in segment
                ),
                "boundary_packets_per_second": dict(
                    sorted(
                        Counter(
                            int(
                                descriptor.completion_time
                                - segment[0].completion_time
                            )
                            for descriptor in segment_boundaries
                        ).items()
                    )
                ),
                "descriptor_start_headers": sum(
                    transfer.headers for transfer in segment
                ),
                "headers_per_second": dict(
                    sorted(
                        Counter(
                            int(
                                descriptor.completion_time
                                - segment[0].completion_time
                            )
                            for descriptor in segment_headers
                        ).items()
                    )
                ),
                "header_timestamps": timestamp_report(
                    segment_headers, ep82_data
                ),
                "ordering_model": simulate_ordering(
                    segment, boundaries_by_transfer, headers_by_transfer
                ),
            }
        )

    audio_segments = []
    current_audio_segment: list[Descriptor] = []
    previous_audio_completion: float | None = None
    for descriptor in descriptors["0x83"]:
        if (
            previous_audio_completion is None
            or descriptor.completion_time - previous_audio_completion
            <= segment_gap
        ):
            current_audio_segment.append(descriptor)
        else:
            audio_segments.append(current_audio_segment)
            current_audio_segment = [descriptor]
        previous_audio_completion = descriptor.completion_time
    if current_audio_segment:
        audio_segments.append(current_audio_segment)

    return {
        "directory": str(directory),
        "boundary_packets": len(boundary_descriptors),
        "boundary_transfers": sum(
            transfer.endpoint == "0x81" and transfer.boundary_packets > 0
            for transfer in transfers.values()
        ),
        "ff_runs_any_offset": len(all_ff_runs),
        "descriptor_start_headers": len(header_descriptors),
        "header_timestamps": timestamp_report(header_descriptors, ep82_data),
        "header_transfers": sum(
            transfer.endpoint == "0x82" and transfer.headers > 0
            for transfer in transfers.values()
        ),
        "ordering_model": simulate_ordering(
            timeline, boundaries_by_transfer, headers_by_transfer
        ),
        "segment_gap_ms": segment_gap_ms,
        "segments": segments,
        "audio_segments": [
            {
                "index": index,
                "start_seconds": segment[0].completion_time - first_completion,
                "duration_seconds": (
                    segment[-1].completion_time
                    - segment[0].completion_time
                ),
                "payload_bytes": sum(item.length for item in segment),
                "payload_duration_seconds_at_16khz_stereo_s16": (
                    sum(item.length for item in segment) / 64000
                ),
            }
            for index, segment in enumerate(audio_segments, start=1)
        ],
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "directories",
        type=Path,
        nargs="+",
        help="one or more extract_sony_stream.py output directories",
    )
    parser.add_argument(
        "--segment-gap-ms",
        type=float,
        default=500,
        help="split streaming intervals after this completion-time gap",
    )
    args = parser.parse_args()
    reports = [
        analyze(directory.resolve(), args.segment_gap_ms)
        for directory in args.directories
    ]
    print(json.dumps(reports, indent=2))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, json.JSONDecodeError) as error:
        import sys

        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1)
