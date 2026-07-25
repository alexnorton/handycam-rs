#!/usr/bin/env python3
"""Extract S_PUB32 symbols from an embedded CodeView NB11 table.

The Sony XP driver ships with old-style CodeView data appended to the PE file.
This helper turns its global-public subsection into a stable TSV address map
for disassembly and decompiler imports.
"""

from __future__ import annotations

import argparse
import csv
import struct
from dataclasses import dataclass
from pathlib import Path


SST_GLOBAL_PUB = 0x012A
S_PUB32 = 0x1009


@dataclass(frozen=True)
class Section:
    number: int
    name: str
    virtual_address: int
    virtual_size: int


def unpack_from(fmt: str, data: bytes, offset: int) -> tuple[int, ...]:
    size = struct.calcsize(fmt)
    if offset < 0 or offset + size > len(data):
        raise ValueError(f"structure at 0x{offset:x} is outside the file")
    return struct.unpack_from(fmt, data, offset)


def pe_sections(data: bytes) -> tuple[int, list[Section]]:
    if data[:2] != b"MZ":
        raise ValueError("input is not an MZ executable")
    (pe_offset,) = unpack_from("<I", data, 0x3C)
    if data[pe_offset : pe_offset + 4] != b"PE\0\0":
        raise ValueError("input has no PE signature")
    coff_offset = pe_offset + 4
    _, section_count, _, _, _, optional_size, _ = unpack_from(
        "<HHIIIHH", data, coff_offset
    )
    optional_offset = coff_offset + 20
    (magic,) = unpack_from("<H", data, optional_offset)
    if magic != 0x10B:
        raise ValueError(f"expected a PE32 optional header, found 0x{magic:x}")
    (image_base,) = unpack_from("<I", data, optional_offset + 28)
    section_offset = optional_offset + optional_size
    sections = []
    for index in range(section_count):
        offset = section_offset + index * 40
        raw_name = data[offset : offset + 8]
        name = raw_name.split(b"\0", 1)[0].decode("ascii", errors="replace")
        virtual_size, virtual_address = unpack_from("<II", data, offset + 8)
        sections.append(
            Section(index + 1, name, virtual_address, virtual_size)
        )
    return image_base, sections


def find_global_publics(data: bytes) -> tuple[int, int]:
    codeview_base = data.find(b"NB11")
    if codeview_base < 0:
        raise ValueError("no CodeView NB11 signature found")
    (directory_relative,) = unpack_from("<I", data, codeview_base + 4)
    directory = codeview_base + directory_relative
    header_size, entry_size, entry_count, _, _ = unpack_from(
        "<HHIII", data, directory
    )
    if header_size < 16 or entry_size < 12:
        raise ValueError("invalid NB11 subsection directory")
    for index in range(entry_count):
        entry = directory + header_size + index * entry_size
        subsection, module, relative, size = unpack_from("<HHII", data, entry)
        if subsection == SST_GLOBAL_PUB and module == 0xFFFF:
            start = codeview_base + relative
            if start + size > len(data):
                raise ValueError("global-public subsection is outside the file")
            return start, size
    raise ValueError("NB11 data has no global-public subsection")


def public_symbols(
    data: bytes, subsection: int, subsection_size: int
) -> list[tuple[int, int, int, str]]:
    if subsection_size < 16:
        raise ValueError("global-public subsection is too short")
    _, _, symbol_bytes, _, _ = unpack_from("<HHIII", data, subsection)
    position = subsection + 16
    end = position + symbol_bytes
    if end > subsection + subsection_size:
        raise ValueError("public symbol stream exceeds its subsection")
    symbols = []
    while position < end:
        (record_length,) = unpack_from("<H", data, position)
        record_end = position + 2 + record_length
        if record_length < 2 or record_end > end:
            raise ValueError(f"bad symbol record at 0x{position:x}")
        (record_type,) = unpack_from("<H", data, position + 2)
        if record_type == S_PUB32:
            flags, section_offset, section_number = unpack_from(
                "<IIH", data, position + 4
            )
            name_length_offset = position + 14
            name_length = data[name_length_offset]
            name_start = name_length_offset + 1
            name_end = name_start + name_length
            if name_end > record_end:
                raise ValueError(f"bad symbol name at 0x{position:x}")
            name = data[name_start:name_end].decode(
                "ascii", errors="backslashreplace"
            )
            symbols.append((section_number, section_offset, flags, name))
        position = (record_end + 3) & ~3
    return symbols


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("driver", type=Path)
    parser.add_argument(
        "-o",
        "--output",
        type=Path,
        help="write TSV here instead of standard output",
    )
    args = parser.parse_args()

    data = args.driver.read_bytes()
    image_base, sections = pe_sections(data)
    section_by_number = {section.number: section for section in sections}
    subsection, size = find_global_publics(data)
    symbols = public_symbols(data, subsection, size)

    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        stream = args.output.open("w", newline="", encoding="utf-8")
    else:
        import sys

        stream = sys.stdout
    try:
        writer = csv.writer(stream, delimiter="\t", lineterminator="\n")
        writer.writerow(
            (
                "image_va",
                "rva",
                "section",
                "section_offset",
                "flags",
                "name",
            )
        )
        for section_number, section_offset, flags, name in sorted(
            symbols,
            key=lambda item: (
                section_by_number.get(
                    item[0], Section(item[0], "", 0, 0)
                ).virtual_address
                + item[1],
                item[3],
            ),
        ):
            section = section_by_number.get(section_number)
            if section is None:
                section_name = f"#{section_number}"
                rva = section_offset
            else:
                section_name = section.name
                rva = section.virtual_address + section_offset
            writer.writerow(
                (
                    f"0x{image_base + rva:08x}",
                    f"0x{rva:08x}",
                    section_name,
                    f"0x{section_offset:08x}",
                    f"0x{flags:08x}",
                    name,
                )
            )
    finally:
        if args.output:
            stream.close()

    if args.output:
        print(f"Wrote {len(symbols)} symbols to {args.output}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except ValueError as error:
        import sys

        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1)
