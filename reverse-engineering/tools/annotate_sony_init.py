#!/usr/bin/env python3
"""Annotate the captured record-mode initialization plan with semantic phases.

Exact labels come from named sonypvs1.sys register tables.  Labels after
DevmanPowerOn additionally use the call order recovered from
ToDeviceCaptureStart; ranges that cannot yet be split confidently remain
explicitly marked as composite or unresolved.
"""

from __future__ import annotations

import argparse
import csv
import sys
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class Phase:
    first_frame: int
    last_frame: int
    name: str
    confidence: str
    basis: str


PHASES = [
    Phase(4198, 4200, "stream_quiesce", "observed", "register 0x27 and alt 0"),
    Phase(4202, 4204, "DevmanPowerOff", "exact", "named static register table"),
    Phase(
        4206,
        4254,
        "pre_stream_setup",
        "unresolved",
        "status, bandwidth alternates, and first mailbox command",
    ),
    Phase(4256, 4262, "DevmanAudioOn", "exact", "named static register table"),
    Phase(
        4264,
        4332,
        "encoder_preconfiguration",
        "unresolved",
        "register setup preceding ToDeviceCaptureStart",
    ),
    Phase(4334, 4378, "DevmanInit", "exact", "named static register table"),
    Phase(4380, 4386, "DevmanPowerOn", "exact", "named static register table"),
    Phase(
        4388,
        4388,
        "DevmanEstimation",
        "inferred",
        "ToDeviceCaptureStart call order and one register write",
    ),
    Phase(
        4390,
        4390,
        "DevmanEnableESFeatures",
        "inferred",
        "ToDeviceCaptureStart call order",
    ),
    Phase(
        4392,
        4414,
        "DevmanDisableSDRAMBuffering",
        "inferred",
        "ToDeviceCaptureStart call order",
    ),
    Phase(
        4416,
        4416,
        "DevmanWriteReg07E",
        "inferred",
        "ToDeviceCaptureStart call order and register 0x7e",
    ),
    Phase(
        4418,
        4420,
        "DevmanTurnOffVideo",
        "inferred",
        "ToDeviceCaptureStart call order",
    ),
    Phase(
        4422,
        4426,
        "DevmanSetColorSpace",
        "inferred",
        "ToDeviceCaptureStart call order",
    ),
    Phase(
        4428,
        4468,
        "DevmanSetRes",
        "inferred",
        "ToDeviceCaptureStart call order",
    ),
    Phase(
        4470,
        4470,
        "DevmanSetFrameRate",
        "high",
        "call order and Apollo register 0 at index 0x2b",
    ),
    Phase(
        4472,
        4488,
        "DevmanWriteQuantizationMatrices",
        "high",
        "call order and two 0x80/0xc0 matrix upload pairs",
    ),
    Phase(
        4490,
        4490,
        "DevmanWriteScalingFactors",
        "exact",
        "simple-hardware write to register 0x7c",
    ),
    Phase(
        4492,
        4492,
        "DevmanSetBanding",
        "high",
        "call order and single register write",
    ),
    Phase(
        4494,
        4520,
        "encoder_start_composite",
        "composite",
        "DevmanHandshake, SetCodeMode, ResetEncoder, and TurnOnVideo",
    ),
    Phase(
        4522,
        4642,
        "startup_mailbox_scan",
        "observed",
        "0x0300 token commands polled through matching status[0]",
    ),
]


def phase_for(frame: int) -> Phase:
    matches = [
        phase for phase in PHASES if phase.first_frame <= frame <= phase.last_frame
    ]
    if len(matches) != 1:
        return Phase(frame, frame, "unclassified", "none", "outside known ranges")
    return matches[0]


def annotate(source: Path, destination: Path) -> tuple[int, set[str]]:
    count = 0
    phase_names = set()
    with source.open(newline="", encoding="utf-8") as input_stream:
        reader = csv.reader(input_stream, delimiter="\t")
        with destination.open("w", newline="", encoding="utf-8") as output_stream:
            writer = csv.writer(output_stream, delimiter="\t", lineterminator="\n")
            writer.writerow(
                [
                    "phase",
                    "confidence",
                    "basis",
                    "delay_us",
                    "bmRequestType",
                    "bRequest",
                    "wValue",
                    "wIndex",
                    "wLength",
                    "data_hex",
                    "capture_frame",
                ]
            )
            for fields in reader:
                if not fields or fields[0].startswith("#"):
                    continue
                if len(fields) != 8:
                    raise ValueError(f"expected 8 TSV fields, found {len(fields)}")
                frame = int(fields[7], 0)
                phase = phase_for(frame)
                writer.writerow(
                    [phase.name, phase.confidence, phase.basis, *fields]
                )
                phase_names.add(phase.name)
                count += 1
    return count, phase_names


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "source",
        nargs="?",
        type=Path,
        default=Path("crates/handycam-core/assets/record-mode-init.tsv"),
    )
    parser.add_argument("output", type=Path)
    return parser.parse_args()


def main() -> int:
    arguments = parse_args()
    try:
        count, phases = annotate(arguments.source, arguments.output)
    except (OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    print(
        f"annotated {count} operations across {len(phases)} phases: "
        f"{arguments.output}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
