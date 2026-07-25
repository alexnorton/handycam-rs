#!/usr/bin/env python3
"""Analyze Sony request-0x88 control transcripts.

This identifies the command-mailbox families, byte-level status variability,
the observed command-token acknowledgement rule, and known semantic
initialization sequences extracted from sonypvs1.sys.
"""

from __future__ import annotations

import argparse
import collections
import json
import statistics
import sys
from pathlib import Path
from typing import Any


POWER_OFF = [(0x35, 0x06), (0x35, 0x00)]
AUDIO_ON = [(0x23, 0x00), (0x24, 0x68), (0x22, 0x0C), (0x35, 0x0F)]
POWER_ON = [(0x35, 0x06), (0x36, 0x01), (0x36, 0x03), (0x35, 0x0F)]
DEVICE_INIT_INDICES = [
    0x00,
    0x50,
    0x55,
    0x61,
    0x7A,
    0x06,
    0x07,
    0x0A,
    0x0F,
    0x20,
    0x21,
    0x53,
    0x54,
    0x5E,
    0x78,
    0x79,
    0x7C,
    0x37,
    0x51,
    0x76,
    0x23,
]


def load_rows(path: Path) -> list[dict[str, Any]]:
    with path.open() as source:
        return [json.loads(line) for line in source if line.strip()]


def sony_vendor_rows(rows: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [
        row
        for row in rows
        if row["request"] == 0x88 and row["request_type"] in (0x40, 0xC0)
    ]


def status_rows(rows: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [
        row
        for row in rows
        if row["request_type"] == 0xC0
        and row["index"] == 0x0340
        and row["length"] == 64
        and row["response_data"]
    ]


def command_rows(rows: list[dict[str, Any]]) -> list[dict[str, Any]]:
    return [
        row
        for row in rows
        if row["request_type"] == 0x40
        and row["index"] == 0x0300
        and row["length"] == 64
        and row["output_data"]
    ]


def command_word(row: dict[str, Any]) -> bytes:
    return bytes.fromhex(row["output_data"])[-4:]


def classify_command(command: bytes) -> str:
    if (
        command[0] == 0
        and command[1] & 0x0F == 0x09
        and command[2] == 0x10
        and command[3] == (command[1] & 0xF0) | 0x01
    ):
        return "scan"
    if (
        command[0] == 0
        and command[1] & 0x0F == 0x01
        and command[2] == 0x18
        and command[3] == command[1] & 0xF0
    ):
        return "paired"
    if command == bytes.fromhex("00188010"):
        return "special_00188010"
    return "other"


def find_contiguous_write_matches(
    rows: list[dict[str, Any]],
    name: str,
    expected: list[tuple[int, int]] | None = None,
    expected_indices: list[int] | None = None,
) -> dict[str, Any]:
    writes = [
        row
        for row in rows
        if row["request_type"] == 0x40
        and row["length"] == 1
        and row["output_data"]
    ]
    wanted_indices = (
        [index for index, _value in expected] if expected is not None else expected_indices
    )
    assert wanted_indices is not None
    matches = []
    for start in range(len(writes) - len(wanted_indices) + 1):
        candidate = writes[start : start + len(wanted_indices)]
        if [row["index"] for row in candidate] != wanted_indices:
            continue
        differences = []
        if expected is not None:
            for offset, (row, (_index, value)) in enumerate(
                zip(candidate, expected, strict=True)
            ):
                observed = bytes.fromhex(row["output_data"])[0]
                if observed != value:
                    differences.append(
                        {
                            "offset": offset,
                            "index": f"0x{row['index']:04x}",
                            "expected": f"0x{value:02x}",
                            "observed": f"0x{observed:02x}",
                        }
                    )
        matches.append(
            {
                "start_frame": candidate[0]["submit_frame"],
                "end_frame": candidate[-1]["submit_frame"],
                "value_differences": differences,
            }
        )
    return {"name": name, "matches": matches}


def analyze_status(statuses: list[dict[str, Any]]) -> dict[str, Any]:
    buffers = [bytes.fromhex(row["response_data"]) for row in statuses]
    cursor_pairs = collections.Counter((buffer[4], buffer[8]) for buffer in buffers)
    equal_cursors = sum(
        count for (left, right), count in cursor_pairs.items() if left == right
    )
    offsets = []
    for offset in range(64):
        counts = collections.Counter(buffer[offset] for buffer in buffers)
        if len(counts) > 1 or (counts and next(iter(counts)) != 0):
            offsets.append(
                {
                    "offset": offset,
                    "distinct_values": len(counts),
                    "values": {
                        f"0x{value:02x}": count
                        for value, count in sorted(counts.items())
                    },
                }
            )
    return {
        "reads": len(statuses),
        "unique_buffers": len(set(buffers)),
        "meaningful_offsets": offsets,
        "observed_invariants": {
            "byte_1": "0x01"
            if buffers and all(buffer[1] == 0x01 for buffer in buffers)
            else None,
            "byte_2": "0x02"
            if buffers and all(buffer[2] == 0x02 for buffer in buffers)
            else None,
            "byte_6": "0x03"
            if buffers and all(buffer[6] == 0x03 for buffer in buffers)
            else None,
            "byte_7": "0x52"
            if buffers and all(buffer[7] == 0x52 for buffer in buffers)
            else None,
            "byte_4_and_byte_8": {
                "equal": equal_cursors,
                "different": len(buffers) - equal_cursors,
                "equality_rate": equal_cursors / len(buffers) if buffers else None,
                "pairs": {
                    f"0x{left:02x}:0x{right:02x}": count
                    for (left, right), count in sorted(cursor_pairs.items())
                },
            },
            "bytes_10_through_63_zero": bool(buffers)
            and all(not any(buffer[10:]) for buffer in buffers),
        },
    }


def analyze_commands(
    rows: list[dict[str, Any]],
    commands: list[dict[str, Any]],
    statuses: list[dict[str, Any]],
) -> dict[str, Any]:
    row_positions = {id(row): index for index, row in enumerate(rows)}
    status_positions = [
        (row_positions[id(row)], row, bytes.fromhex(row["response_data"]))
        for row in statuses
    ]
    class_counts: collections.Counter[str] = collections.Counter()
    unique_by_class: dict[str, collections.Counter[str]] = collections.defaultdict(
        collections.Counter
    )
    scan_total = 0
    scan_acknowledged = 0
    scan_latencies = []

    for command_index, row in enumerate(commands):
        command = command_word(row)
        classification = classify_command(command)
        class_counts[classification] += 1
        unique_by_class[classification][command.hex()] += 1
        if classification != "scan":
            continue
        scan_total += 1
        position = row_positions[id(row)]
        next_position = (
            row_positions[id(commands[command_index + 1])]
            if command_index + 1 < len(commands)
            else len(rows)
        )
        acknowledgement = next(
            (
                status
                for status_position, status, buffer in status_positions
                if position < status_position < next_position
                and buffer[0] == command[3]
            ),
            None,
        )
        if acknowledgement is not None:
            scan_acknowledged += 1
            scan_latencies.append(
                round(
                    (
                        float(acknowledgement["complete_time_epoch"])
                        - float(row["submit_time_epoch"])
                    )
                    * 1_000_000
                )
            )

    return {
        "writes": len(commands),
        "classes": {
            name: {
                "count": class_counts[name],
                "unique_commands": dict(sorted(unique_by_class[name].items())),
            }
            for name in sorted(class_counts)
        },
        "scan_acknowledgement": {
            "rule": "status[0] == command[3]",
            "scope": "before the next 0x0300 command",
            "commands": scan_total,
            "acknowledged": scan_acknowledged,
            "rate": scan_acknowledged / scan_total if scan_total else None,
            "latency_us": {
                "minimum": min(scan_latencies, default=None),
                "median": statistics.median(scan_latencies)
                if scan_latencies
                else None,
                "p95": (
                    sorted(scan_latencies)[
                        min(
                            len(scan_latencies) - 1,
                            round(0.95 * (len(scan_latencies) - 1)),
                        )
                    ]
                    if scan_latencies
                    else None
                ),
                "maximum": max(scan_latencies, default=None),
            },
        },
    }


def analyze(rows: list[dict[str, Any]]) -> dict[str, Any]:
    vendor = sony_vendor_rows(rows)
    statuses = status_rows(vendor)
    commands = command_rows(vendor)
    index_counts = collections.Counter(
        (row["request_type"], row["index"], row["length"]) for row in vendor
    )
    return {
        "schema": 1,
        "transactions": len(rows),
        "sony_vendor_transactions": len(vendor),
        "request_shapes": [
            {
                "direction": "in" if request_type & 0x80 else "out",
                "index": f"0x{index:04x}",
                "length": length,
                "count": count,
            }
            for (request_type, index, length), count in sorted(
                index_counts.items(), key=lambda item: (-item[1], item[0])
            )
        ],
        "status_0340": analyze_status(statuses),
        "commands_0300": analyze_commands(vendor, commands, statuses),
        "semantic_sequence_matches": [
            find_contiguous_write_matches(vendor, "DevmanPowerOff", POWER_OFF),
            find_contiguous_write_matches(vendor, "DevmanAudioOn", AUDIO_ON),
            find_contiguous_write_matches(
                vendor,
                "DevmanInit",
                expected_indices=DEVICE_INIT_INDICES,
            ),
            find_contiguous_write_matches(vendor, "DevmanPowerOn", POWER_ON),
        ],
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("transcript", type=Path)
    parser.add_argument("--compact", action="store_true")
    return parser.parse_args()


def main() -> int:
    arguments = parse_args()
    try:
        rows = load_rows(arguments.transcript)
        result = analyze(rows)
    except (OSError, ValueError, KeyError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(json.dumps(result, indent=None if arguments.compact else 2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
