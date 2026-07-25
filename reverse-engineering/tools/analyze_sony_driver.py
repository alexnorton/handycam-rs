#!/usr/bin/env python3
"""Extract protocol-relevant data from Sony's sonypvs1.sys.

This is deliberately a small PE reader rather than a general decompiler.  It
maps CodeView symbol virtual addresses back into the driver image and decodes
the static register/value tables used by the named Devman routines.  Output is
JSON so findings can be diffed and consumed by other analysis tools.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import struct
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any


@dataclass(frozen=True)
class Section:
    name: str
    virtual_address: int
    virtual_size: int
    raw_offset: int
    raw_size: int


class PeImage:
    def __init__(self, path: Path) -> None:
        self.path = path
        self.data = path.read_bytes()
        if self.data[:2] != b"MZ":
            raise ValueError(f"{path}: missing DOS MZ signature")
        pe_offset = struct.unpack_from("<I", self.data, 0x3C)[0]
        if self.data[pe_offset : pe_offset + 4] != b"PE\0\0":
            raise ValueError(f"{path}: missing PE signature")

        coff_offset = pe_offset + 4
        (
            self.machine,
            section_count,
            _timestamp,
            _symbol_table,
            _symbol_count,
            optional_size,
            _characteristics,
        ) = struct.unpack_from("<HHIIIHH", self.data, coff_offset)
        optional_offset = coff_offset + 20
        magic = struct.unpack_from("<H", self.data, optional_offset)[0]
        if magic != 0x10B:
            raise ValueError(f"{path}: expected PE32, found magic {magic:#x}")
        self.image_base = struct.unpack_from("<I", self.data, optional_offset + 28)[0]

        section_offset = optional_offset + optional_size
        sections = []
        for index in range(section_count):
            offset = section_offset + index * 40
            name = self.data[offset : offset + 8].split(b"\0", 1)[0].decode("ascii")
            virtual_size, virtual_address, raw_size, raw_offset = struct.unpack_from(
                "<IIII", self.data, offset + 8
            )
            sections.append(
                Section(
                    name=name,
                    virtual_address=virtual_address,
                    virtual_size=virtual_size,
                    raw_offset=raw_offset,
                    raw_size=raw_size,
                )
            )
        self.sections = sections

    def read_va(self, address: int, size: int) -> bytes:
        rva = address - self.image_base
        for section in self.sections:
            extent = max(section.virtual_size, section.raw_size)
            if section.virtual_address <= rva and rva + size <= section.virtual_address + extent:
                relative = rva - section.virtual_address
                if relative + size > section.raw_size:
                    raise ValueError(
                        f"read at {address:#x} extends into zero-filled section data"
                    )
                start = section.raw_offset + relative
                return self.data[start : start + size]
        raise ValueError(f"virtual address {address:#x} is not mapped")


def load_symbols(path: Path) -> dict[str, int]:
    symbols: dict[str, int] = {}
    with path.open(newline="") as source:
        for fields in csv.reader(source, delimiter="\t"):
            if not fields or fields[0].startswith("#") or fields[0] == "image_va":
                continue
            if len(fields) < 6:
                raise ValueError(f"{path}: malformed symbol row {fields!r}")
            symbols[fields[5]] = int(fields[0], 16)
    return symbols


def symbol_address(symbols: dict[str, int], needle: str) -> int:
    if needle.startswith("Reg"):
        decorated = f"?{needle}@@"
        matches = [
            (name, address) for name, address in symbols.items() if decorated in name
        ]
    else:
        matches = [(name, address) for name, address in symbols.items() if needle in name]
    if len(matches) != 1:
        names = ", ".join(name for name, _address in matches) or "none"
        raise ValueError(f"expected one symbol containing {needle!r}, found {names}")
    return matches[0][1]


def decode_sequence(
    image: PeImage,
    symbols: dict[str, int],
    name: str,
    index_symbol: str,
    value_symbol: str,
    count: int,
    value_stride: int,
) -> dict[str, Any]:
    index_address = symbol_address(symbols, index_symbol)
    value_address = symbol_address(symbols, value_symbol)
    indices = struct.unpack(
        f"<{count}H", image.read_va(index_address, count * 2)
    )
    value_data = image.read_va(value_address, count * value_stride)
    values = [value_data[offset * value_stride] for offset in range(count)]
    return {
        "name": name,
        "index_symbol": index_symbol,
        "value_symbol": value_symbol,
        "index_address": f"0x{index_address:08x}",
        "value_address": f"0x{value_address:08x}",
        "operations": [
            {"index": f"0x{index:04x}", "value": f"0x{value:02x}"}
            for index, value in zip(indices, values, strict=True)
        ],
    }


def load_plan(path: Path) -> list[dict[str, Any]]:
    operations = []
    with path.open(newline="") as source:
        for line_number, fields in enumerate(csv.reader(source, delimiter="\t"), 1):
            if not fields or fields[0].startswith("#"):
                continue
            if len(fields) != 8:
                raise ValueError(f"{path}:{line_number}: expected 8 TSV fields")
            operations.append(
                {
                    "request_type": int(fields[1], 0),
                    "request": int(fields[2], 0),
                    "index": int(fields[4], 0),
                    "length": int(fields[5], 0),
                    "data": None if fields[6] == "-" else bytes.fromhex(fields[6]),
                    "capture_frame": int(fields[7], 0),
                }
            )
    return operations


def match_sequences(
    sequences: list[dict[str, Any]], plan: list[dict[str, Any]]
) -> list[dict[str, Any]]:
    writes = [
        operation
        for operation in plan
        if operation["request_type"] == 0x40
        and operation["request"] == 0x88
        and operation["length"] == 1
        and operation["data"] is not None
    ]
    results = []
    for sequence in sequences:
        expected = [
            (int(operation["index"], 0), int(operation["value"], 0))
            for operation in sequence["operations"]
        ]
        matches = []
        for start in range(len(writes) - len(expected) + 1):
            candidate = writes[start : start + len(expected)]
            if [operation["index"] for operation in candidate] != [
                index for index, _value in expected
            ]:
                continue
            differences = []
            for offset, (operation, (_index, expected_value)) in enumerate(
                zip(candidate, expected, strict=True)
            ):
                observed_value = operation["data"][0]
                if observed_value != expected_value:
                    differences.append(
                        {
                            "offset": offset,
                            "index": f"0x{operation['index']:04x}",
                            "static_value": f"0x{expected_value:02x}",
                            "observed_value": f"0x{observed_value:02x}",
                        }
                    )
            matches.append(
                {
                    "start_capture_frame": candidate[0]["capture_frame"],
                    "end_capture_frame": candidate[-1]["capture_frame"],
                    "value_differences": differences,
                }
            )
        results.append({"name": sequence["name"], "matches": matches})
    return results


def analyze(
    image: PeImage, symbols: dict[str, int], plan: list[dict[str, Any]] | None
) -> dict[str, Any]:
    # Counts and strides come from the loop bounds in the named routines.
    sequences = [
        decode_sequence(
            image,
            symbols,
            "DevmanInit",
            "RegIdxInit",
            "RegValInit",
            21,
            2,
        ),
        decode_sequence(
            image,
            symbols,
            "DevmanAudioOn",
            "RegIdxAudioOn",
            "RegValAudioOn",
            4,
            1,
        ),
        decode_sequence(
            image,
            symbols,
            "DevmanPowerOn",
            "RegIdxPowerOn",
            "RegValPowerOn",
            4,
            1,
        ),
        decode_sequence(
            image,
            symbols,
            "DevmanPowerOff",
            "RegIdxPowerOff",
            "RegValPowerOff",
            2,
            1,
        ),
    ]

    # ApolloRead/Write index this address directly.  It sits in an otherwise
    # data-like part of .text rather than having its own CodeView symbol.
    apollo_register_map_address = 0x0001B080
    apollo_register_map = list(image.read_va(apollo_register_map_address, 4))

    routine_needles = [
        "_DevmanInit",
        "_DevmanAudioOn",
        "_DevmanWriteScalingFactors",
        "_DevmanWriteQuantizationMatrices",
        "_DevmanSetFrameRate",
        "?CustomPropCommandWrite",
        "?CustomPropApolloRegister",
        "?ToDeviceWriteReg@@",
        "?ToDeviceWriteRegs@@",
    ]
    routines = {}
    for needle in routine_needles:
        address = symbol_address(symbols, needle)
        routines[needle] = f"0x{address:08x}"

    result = {
        "schema": 1,
        "driver": {
            "path": str(image.path),
            "sha256": hashlib.sha256(image.data).hexdigest(),
            "machine": f"0x{image.machine:04x}",
            "image_base": f"0x{image.image_base:08x}",
            "sections": [
                {
                    "name": section.name,
                    "virtual_address": f"0x{image.image_base + section.virtual_address:08x}",
                    "virtual_size": section.virtual_size,
                    "raw_offset": section.raw_offset,
                    "raw_size": section.raw_size,
                }
                for section in image.sections
            ],
        },
        "routines": routines,
        "apollo_register_map": {
            "address": f"0x{apollo_register_map_address:08x}",
            "values": [f"0x{value:02x}" for value in apollo_register_map],
        },
        "register_sequences": sequences,
        "derived_protocol": {
            "command_block": {
                "index": "0x0300",
                "length": 64,
                "command_offset": 60,
                "encoding": "u32 big-endian",
            },
            "scale_factor": {
                "simple_hardware_index": "0x007c",
                "encoding": "u8",
                "configured_default": 36,
                "configured_minimum": 4,
                "configured_maximum": 128,
            },
            "audio": {
                "endpoint": "0x83",
                "format": "signed PCM16LE",
                "channels": 2,
                "sample_rate_hz": 16000,
                "bytes_per_usb_frame": 64,
            },
        },
    }
    if plan is not None:
        result["initialization_plan_matches"] = match_sequences(sequences, plan)
    return result


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--driver",
        type=Path,
        default=Path("reverse-engineering/artifacts/sony-driver/sonypvs1.sys"),
    )
    parser.add_argument(
        "--symbols",
        type=Path,
        default=Path("reverse-engineering/artifacts/sony-driver/sonypvs1-symbols.tsv"),
    )
    parser.add_argument(
        "--plan",
        type=Path,
        default=Path("crates/handycam-core/assets/record-mode-init.tsv"),
        help="compare extracted sequences with this captured initialization plan",
    )
    parser.add_argument(
        "--no-plan",
        action="store_true",
        help="do not load or compare an initialization plan",
    )
    parser.add_argument("--compact", action="store_true")
    return parser.parse_args()


def main() -> int:
    arguments = parse_args()
    try:
        image = PeImage(arguments.driver)
        symbols = load_symbols(arguments.symbols)
        plan = None if arguments.no_plan else load_plan(arguments.plan)
        result = analyze(image, symbols, plan)
    except (OSError, ValueError, struct.error) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    indent = None if arguments.compact else 2
    print(json.dumps(result, indent=indent, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
