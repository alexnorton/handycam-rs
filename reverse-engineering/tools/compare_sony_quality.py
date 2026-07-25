#!/usr/bin/env python3
"""Compare encoder-quality register activity in two Sony control transcripts.

The DCR-HC24 uses register 0x007c as a live JPEG scale factor.  This tool
extracts its write history, identifies the common reset/configure pairs, and
checks whether the 0x0080/0x00c0 quantization matrices changed between traces.
"""

from __future__ import annotations

import argparse
import collections
import hashlib
import json
import statistics
import sys
from pathlib import Path
from typing import Any


SCALE_FACTOR_INDEX = 0x007C
MATRIX_INDICES = (0x0080, 0x00C0)
RESET_SCALE_FACTOR = 0x10


def load_rows(path: Path) -> list[dict[str, Any]]:
    with path.open(encoding="utf-8") as source:
        return [json.loads(line) for line in source if line.strip()]


def writes(rows: list[dict[str, Any]], index: int) -> list[dict[str, Any]]:
    return [
        row
        for row in rows
        if row.get("request_type") == 0x40
        and row.get("request") == 0x88
        and row.get("index") == index
        and row.get("output_data")
    ]


def byte_write_history(rows: list[dict[str, Any]], index: int) -> list[dict[str, Any]]:
    single_byte_writes = [
        row
        for row in rows
        if row.get("request_type") == 0x40
        and row.get("request") == 0x88
        and row.get("output_data")
        and len(bytes.fromhex(row["output_data"])) == 1
    ]
    result = []
    for position, row in enumerate(single_byte_writes):
        if row.get("index") != index:
            continue
        payload = bytes.fromhex(row["output_data"])
        previous_index = (
            single_byte_writes[position - 1].get("index") if position else None
        )
        result.append(
            {
                "frame": int(row["submit_frame"]),
                "time_epoch": float(row["submit_time_epoch"]),
                "value": payload[0],
                "value_hex": f"0x{payload[0]:02x}",
                "follows_init_register_0x0079": previous_index == 0x0079,
            }
        )
    return result


def reset_configure_pairs(history: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Pair reset value 0x10 with the next non-reset scale write.

    The reset and configured writes are normally separated by many other
    register transactions, so pairing adjacent scale-factor writes is the
    useful view.
    """

    pairs = []
    for position, item in enumerate(history):
        if (
            item["value"] != RESET_SCALE_FACTOR
            or not item["follows_init_register_0x0079"]
        ):
            continue
        configured = next(
            (
                candidate
                for candidate in history[position + 1 :]
                if candidate["value"] != RESET_SCALE_FACTOR
            ),
            None,
        )
        if configured is None:
            continue
        pairs.append(
            {
                "reset_frame": item["frame"],
                "configured_frame": configured["frame"],
                "configured_value": configured["value"],
                "configured_value_hex": configured["value_hex"],
            }
        )
    return pairs


def matrix_summary(rows: list[dict[str, Any]]) -> dict[str, Any]:
    by_index: dict[str, Any] = {}
    for index in MATRIX_INDICES:
        payloads = [
            bytes.fromhex(row["output_data"])
            for row in writes(rows, index)
            if len(bytes.fromhex(row["output_data"])) == 64
        ]
        counts = collections.Counter(
            hashlib.sha256(payload).hexdigest() for payload in payloads
        )
        by_index[f"0x{index:04x}"] = {
            "writes": len(payloads),
            "unique_payloads": len(counts),
            "sha256_counts": dict(sorted(counts.items())),
        }
    return by_index


def summarize(path: Path, rows: list[dict[str, Any]]) -> dict[str, Any]:
    history = byte_write_history(rows, SCALE_FACTOR_INDEX)
    values = [item["value"] for item in history]
    non_reset = [value for value in values if value != RESET_SCALE_FACTOR]
    pairs = reset_configure_pairs(history)
    return {
        "transcript": str(path),
        "scale_factor_register": "0x007c",
        "write_count": len(history),
        "history": history,
        "value_counts": {
            f"0x{value:02x}": count
            for value, count in sorted(collections.Counter(values).items())
        },
        "non_reset": {
            "minimum": min(non_reset, default=None),
            "median": statistics.median(non_reset) if non_reset else None,
            "maximum": max(non_reset, default=None),
        },
        "reset_configure_pairs": pairs,
        "configured_values": [pair["configured_value"] for pair in pairs],
        "quantization_matrices": matrix_summary(rows),
    }


def matrix_hashes(summary: dict[str, Any], index: str) -> set[str]:
    return set(
        summary["quantization_matrices"][index]["sha256_counts"]
    )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("baseline", type=Path)
    parser.add_argument("comparison", type=Path)
    return parser.parse_args()


def main() -> int:
    arguments = parse_args()
    try:
        baseline = summarize(arguments.baseline, load_rows(arguments.baseline))
        comparison = summarize(
            arguments.comparison, load_rows(arguments.comparison)
        )
    except (OSError, ValueError, KeyError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    result = {
        "schema": 1,
        "baseline": baseline,
        "comparison": comparison,
        "cross_trace": {
            "matrix_payloads_equal": {
                index: matrix_hashes(baseline, index)
                == matrix_hashes(comparison, index)
                for index in (f"0x{value:04x}" for value in MATRIX_INDICES)
            },
            "interpretation": (
                "The scale-factor register changes independently of the "
                "quantization-matrix payloads."
            ),
        },
    }
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
