#!/usr/bin/env python3
"""Extract a replayable Sony 0x88 initialization plan from a usbmon pcap.

The output is a tab-separated text file consumed by handycam-capture. It
contains vendor control submissions and interface-0 SET_INTERFACE requests in
capture order, with the delay since the preceding retained request.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path


FIELDS = (
    "frame.number",
    "frame.time_relative",
    "usb.bmRequestType",
    "usb.setup.bRequest",
    "usb.setup.wValue",
    "usb.setup.wIndex",
    "usb.setup.wInterface",
    "usb.setup.wLength",
    "usb.data_fragment",
)

# Wireshark renders SET_INTERFACE's alternate setting in the packet tree but
# does not expose it through usb.setup.wValue for these Linux-usbmon packets.
# These values are taken directly from each frame's eight setup bytes.
KNOWN_SET_INTERFACE = {
    4196: (2, 0),
    4200: (0, 0),
    4240: (0, 7),
    4246: (0, 5),
    4644: (2, 1),
}


def tshark_rows(
    pcap: Path, device_address: int, start_frame: int, end_frame: int
) -> list[list[str]]:
    display_filter = (
        f"frame.number >= {start_frame} && frame.number <= {end_frame} && "
        f"usb.device_address == {device_address} && "
        "usb.urb_type == 0x53 && "
        "(usb.setup.bRequest == 136 || usb.setup.bRequest == 11)"
    )
    command = [
        "tshark",
        "-r",
        str(pcap),
        "-Y",
        display_filter,
        "-T",
        "fields",
        "-E",
        "separator=\t",
    ]
    for field in FIELDS:
        command.extend(("-e", field))
    try:
        result = subprocess.run(
            command,
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
    except FileNotFoundError:
        raise RuntimeError("tshark was not found in PATH") from None
    except subprocess.CalledProcessError as error:
        raise RuntimeError(error.stderr.strip() or "tshark failed") from error
    rows = []
    for line_number, line in enumerate(result.stdout.splitlines(), 1):
        row = line.split("\t")
        if len(row) != len(FIELDS):
            raise RuntimeError(
                f"row {line_number}: got {len(row)} fields, "
                f"expected {len(FIELDS)}"
            )
        rows.append(row)
    return rows


def parse_int(value: str) -> int:
    return int(value, 0)


def extract(
    pcap: Path,
    output: Path,
    device_address: int,
    start_frame: int,
    end_frame: int,
) -> tuple[int, int]:
    rows = tshark_rows(pcap, device_address, start_frame, end_frame)
    previous_time: float | None = None
    retained = 0
    skipped_interfaces = 0

    with output.open("x", encoding="ascii") as stream:
        stream.write(
            "# delay_us\tbmRequestType\tbRequest\twValue\twIndex"
            "\twLength\tdata_hex\tcapture_frame\n"
        )
        for row in rows:
            (
                frame_text,
                time_text,
                request_type_text,
                request_text,
                value_text,
                index_text,
                interface_text,
                length_text,
                data_hex,
            ) = row
            frame = parse_int(frame_text)
            timestamp = float(time_text)
            request_type = parse_int(request_type_text)
            request = parse_int(request_text)

            if request == 11:
                try:
                    interface, alternate = KNOWN_SET_INTERFACE[frame]
                except KeyError as error:
                    raise RuntimeError(
                        f"SET_INTERFACE frame {frame} is not in "
                        "KNOWN_SET_INTERFACE"
                    ) from error
                if interface != 0:
                    skipped_interfaces += 1
                    continue
                value = alternate
                index = interface
                length = 0
                data_hex = "-"
            elif request == 136:
                if request_type not in (0x40, 0xC0):
                    raise RuntimeError(
                        f"frame {frame}: unexpected 0x88 request type "
                        f"0x{request_type:02x}"
                    )
                value = parse_int(value_text)
                index = parse_int(index_text)
                length = parse_int(length_text)
                if request_type == 0x40:
                    if len(data_hex) != length * 2:
                        raise RuntimeError(
                            f"frame {frame}: expected {length} output bytes, "
                            f"got {len(data_hex) // 2}"
                        )
                else:
                    data_hex = "-"
            else:
                continue

            delay_us = (
                0
                if previous_time is None
                else max(0, round((timestamp - previous_time) * 1_000_000))
            )
            previous_time = timestamp
            stream.write(
                f"{delay_us}\t0x{request_type:02x}\t0x{request:02x}"
                f"\t0x{value:04x}\t0x{index:04x}\t{length}"
                f"\t{data_hex or '-'}\t{frame}\n"
            )
            retained += 1
    return retained, skipped_interfaces


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("pcap", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--device-address", type=parse_int, required=True)
    parser.add_argument("--start-frame", type=parse_int, required=True)
    parser.add_argument("--end-frame", type=parse_int, required=True)
    args = parser.parse_args()

    if not args.pcap.is_file():
        parser.error(f"pcap does not exist: {args.pcap}")
    if args.output.exists():
        parser.error(f"output already exists: {args.output}")
    retained, skipped = extract(
        args.pcap,
        args.output,
        args.device_address,
        args.start_frame,
        args.end_frame,
    )
    print(
        f"Wrote {retained} requests to {args.output}; "
        f"skipped {skipped} non-video SET_INTERFACE requests"
    )
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1)
