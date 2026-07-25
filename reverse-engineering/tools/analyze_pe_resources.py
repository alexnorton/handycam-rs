#!/usr/bin/env python3
"""List PE resources and decode classic Win32 dialog controls.

This intentionally implements only the small PE32/resource subset needed by
the preserved Sony Picture Package binaries.  It keeps the static UI evidence
reproducible without requiring a Windows resource editor.
"""

from __future__ import annotations

import argparse
import json
import struct
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any


RESOURCE_TYPES = {
    1: "cursor",
    2: "bitmap",
    3: "icon",
    4: "menu",
    5: "dialog",
    6: "string",
    9: "accelerator",
    10: "rcdata",
    12: "group_cursor",
    14: "group_icon",
    16: "version",
    24: "manifest",
}


@dataclass(frozen=True)
class Section:
    virtual_address: int
    virtual_size: int
    raw_offset: int
    raw_size: int


class PeResources:
    def __init__(self, path: Path) -> None:
        self.path = path
        self.data = path.read_bytes()
        if self.data[:2] != b"MZ":
            raise ValueError(f"{path}: missing DOS MZ signature")
        pe_offset = struct.unpack_from("<I", self.data, 0x3C)[0]
        if self.data[pe_offset : pe_offset + 4] != b"PE\0\0":
            raise ValueError(f"{path}: missing PE signature")
        coff_offset = pe_offset + 4
        section_count = struct.unpack_from("<H", self.data, coff_offset + 2)[0]
        optional_size = struct.unpack_from("<H", self.data, coff_offset + 16)[0]
        optional_offset = coff_offset + 20
        if struct.unpack_from("<H", self.data, optional_offset)[0] != 0x10B:
            raise ValueError(f"{path}: only PE32 images are supported")
        resource_rva, resource_size = struct.unpack_from(
            "<II", self.data, optional_offset + 96 + 2 * 8
        )
        section_offset = optional_offset + optional_size
        self.sections = []
        for index in range(section_count):
            offset = section_offset + index * 40
            virtual_size, virtual_address, raw_size, raw_offset = struct.unpack_from(
                "<IIII", self.data, offset + 8
            )
            self.sections.append(
                Section(virtual_address, virtual_size, raw_offset, raw_size)
            )
        self.resource_rva = resource_rva
        self.resource_size = resource_size
        self.resource_offset = self.rva_to_offset(resource_rva, resource_size)

    def rva_to_offset(self, rva: int, size: int = 1) -> int:
        for section in self.sections:
            extent = max(section.virtual_size, section.raw_size)
            if (
                section.virtual_address <= rva
                and rva + size <= section.virtual_address + extent
            ):
                relative = rva - section.virtual_address
                if relative + size > section.raw_size:
                    raise ValueError(f"RVA {rva:#x} extends into zero-filled data")
                return section.raw_offset + relative
        raise ValueError(f"RVA {rva:#x} is not mapped")

    def resource_relative(self, relative: int, size: int) -> bytes:
        if relative < 0 or relative + size > self.resource_size:
            raise ValueError(f"resource-relative read {relative:#x}+{size:#x} invalid")
        start = self.resource_offset + relative
        return self.data[start : start + size]

    def resource_name(self, value: int) -> str | int:
        if not value & 0x80000000:
            return value
        relative = value & 0x7FFFFFFF
        length = struct.unpack("<H", self.resource_relative(relative, 2))[0]
        return self.resource_relative(relative + 2, length * 2).decode("utf-16le")

    def leaves(self) -> list[dict[str, Any]]:
        leaves: list[dict[str, Any]] = []

        def walk(relative: int, path: list[str | int]) -> None:
            header = self.resource_relative(relative, 16)
            named, numbered = struct.unpack_from("<HH", header, 12)
            for index in range(named + numbered):
                name, target = struct.unpack(
                    "<II", self.resource_relative(relative + 16 + index * 8, 8)
                )
                component = self.resource_name(name)
                if target & 0x80000000:
                    walk(target & 0x7FFFFFFF, [*path, component])
                    continue
                data_rva, size, codepage, _reserved = struct.unpack(
                    "<IIII", self.resource_relative(target, 16)
                )
                data_offset = self.rva_to_offset(data_rva, size)
                leaves.append(
                    {
                        "path": [*path, component],
                        "rva": data_rva,
                        "size": size,
                        "codepage": codepage,
                        "data": self.data[data_offset : data_offset + size],
                    }
                )

        walk(0, [])
        return leaves


class DialogReader:
    def __init__(self, data: bytes) -> None:
        self.data = data
        self.offset = 0

    def u16(self) -> int:
        value = struct.unpack_from("<H", self.data, self.offset)[0]
        self.offset += 2
        return value

    def u32(self) -> int:
        value = struct.unpack_from("<I", self.data, self.offset)[0]
        self.offset += 4
        return value

    def align4(self) -> None:
        self.offset = (self.offset + 3) & ~3

    def sz_or_ordinal(self) -> str | int | None:
        first = self.u16()
        if first == 0:
            return None
        if first == 0xFFFF:
            return self.u16()
        units = [first]
        while True:
            unit = self.u16()
            if unit == 0:
                break
            units.append(unit)
        return struct.pack(f"<{len(units)}H", *units).decode(
            "utf-16le", errors="replace"
        )


def decode_dialog(data: bytes) -> dict[str, Any]:
    reader = DialogReader(data)
    extended = (
        len(data) >= 4
        and struct.unpack_from("<H", data, 0)[0] == 1
        and struct.unpack_from("<H", data, 2)[0] == 0xFFFF
    )
    if extended:
        reader.u16()
        reader.u16()
        help_id = reader.u32()
        ex_style = reader.u32()
        style = reader.u32()
        item_count = reader.u16()
    else:
        help_id = None
        style = reader.u32()
        ex_style = reader.u32()
        item_count = reader.u16()
    x, y, width, height = (reader.u16() for _ in range(4))
    menu = reader.sz_or_ordinal()
    window_class = reader.sz_or_ordinal()
    title = reader.sz_or_ordinal()
    font = None
    if style & 0x40:  # DS_SETFONT
        point_size = reader.u16()
        weight = reader.u16() if extended else None
        italic = reader.u16() & 0xFF if extended else None
        font = {
            "point_size": point_size,
            "weight": weight,
            "italic": italic,
            "face": reader.sz_or_ordinal(),
        }

    controls = []
    for _index in range(item_count):
        reader.align4()
        if extended:
            control_help = reader.u32()
            control_ex_style = reader.u32()
            control_style = reader.u32()
            control_x, control_y, control_width, control_height = (
                reader.u16() for _ in range(4)
            )
            control_id = reader.u32()
        else:
            control_help = None
            control_style = reader.u32()
            control_ex_style = reader.u32()
            control_x, control_y, control_width, control_height, control_id = (
                reader.u16() for _ in range(5)
            )
        control_class = reader.sz_or_ordinal()
        control_title = reader.sz_or_ordinal()
        extra_size = reader.u16()
        extra = data[reader.offset : reader.offset + extra_size]
        reader.offset += extra_size
        controls.append(
            {
                "id": control_id,
                "x": control_x,
                "y": control_y,
                "width": control_width,
                "height": control_height,
                "class": control_class,
                "title": control_title,
                "style": f"0x{control_style:08x}",
                "ex_style": f"0x{control_ex_style:08x}",
                "help_id": control_help,
                "extra": extra.hex() or None,
            }
        )
    return {
        "extended": extended,
        "help_id": help_id,
        "x": x,
        "y": y,
        "width": width,
        "height": height,
        "menu": menu,
        "class": window_class,
        "title": title,
        "style": f"0x{style:08x}",
        "ex_style": f"0x{ex_style:08x}",
        "font": font,
        "controls": controls,
    }


def display_component(component: str | int, depth: int) -> str | int:
    if depth == 0 and isinstance(component, int):
        return RESOURCE_TYPES.get(component, component)
    return component


def analyze(path: Path) -> dict[str, Any]:
    image = PeResources(path)
    resources = []
    for leaf in image.leaves():
        path_components = [
            display_component(component, depth)
            for depth, component in enumerate(leaf["path"])
        ]
        result = {
            "path": path_components,
            "rva": f"0x{leaf['rva']:08x}",
            "size": leaf["size"],
            "codepage": leaf["codepage"],
        }
        if path_components and path_components[0] == "dialog":
            result["dialog"] = decode_dialog(leaf["data"])
        resources.append(result)
    return {"file": str(path), "resources": resources}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("image", type=Path)
    parser.add_argument("--dialogs-only", action="store_true")
    args = parser.parse_args()
    result = analyze(args.image)
    if args.dialogs_only:
        result["resources"] = [
            resource
            for resource in result["resources"]
            if resource["path"][0] == "dialog"
        ]
    json.dump(result, sys.stdout, indent=2, ensure_ascii=False)
    print()
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, struct.error) as error:
        print(error, file=sys.stderr)
        raise SystemExit(1)
