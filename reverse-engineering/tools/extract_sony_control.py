#!/usr/bin/env python3
"""Extract paired Sony USB control transactions from a usbmon pcapng.

Unlike the initialization-plan extractor, this retains device-to-host
response payloads and pairs each URB submission with its completion. Output is
JSON Lines so status buffers can be correlated with preceding writes.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Any, Iterator


FIELDS = (
    "frame.number",
    "frame.time_epoch",
    "usb.urb_type",
    "usb.urb_id",
    "usb.device_address",
    "usb.endpoint_address",
    "usb.transfer_type",
    "usb.urb_status",
    "usb.bmRequestType",
    "usb.setup.bRequest",
    "usb.setup.wValue",
    "usb.setup.wIndex",
    "usb.setup.wLength",
    "usb.data_fragment",
    "usb.control.Response",
    "usb.urb_len",
    "usb.data_len",
)


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def parse_int(value: str) -> int:
    return int(value, 0)


def optional_int(value: str, default: int = 0) -> int:
    return parse_int(value) if value else default


def run_tshark(pcap: Path, device_address: int | None) -> Iterator[list[str]]:
    display_filter = "usb.transfer_type == 0x02"
    if device_address is not None:
        display_filter += f" && usb.device_address == {device_address}"
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
        "-E",
        "occurrence=f",
    ]
    for field in FIELDS:
        command.extend(("-e", field))
    try:
        process = subprocess.Popen(
            command,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
    except FileNotFoundError:
        raise RuntimeError("tshark was not found in PATH") from None
    assert process.stdout is not None
    assert process.stderr is not None
    for line_number, line in enumerate(process.stdout, 1):
        row = line.rstrip("\n").split("\t")
        if len(row) != len(FIELDS):
            process.kill()
            raise RuntimeError(
                f"tshark row {line_number}: got {len(row)} fields, "
                f"expected {len(FIELDS)}"
            )
        yield row
    stderr = process.stderr.read().strip()
    return_code = process.wait()
    if return_code:
        raise RuntimeError(f"tshark failed: {stderr or return_code}")


def stage_capture(source: Path, directory: Path) -> Path:
    staged = directory / source.name
    shutil.copyfile(source, staged)
    return staged


def extract(
    source: Path,
    output: Path,
    device_address: int | None,
) -> dict[str, Any]:
    pending: dict[str, dict[str, Any]] = {}
    transactions = []
    addresses: set[int] = set()
    with tempfile.TemporaryDirectory(prefix="handycam-control-") as temporary:
        staged = stage_capture(source, Path(temporary))
        for row in run_tshark(staged, device_address):
            (
                frame_text,
                time_text,
                urb_type,
                urb_id,
                address_text,
                endpoint_text,
                transfer_type_text,
                status_text,
                request_type_text,
                request_text,
                value_text,
                index_text,
                length_text,
                output_data,
                response_data,
                urb_length_text,
                data_length_text,
            ) = row
            frame = parse_int(frame_text)
            address = parse_int(address_text)
            addresses.add(address)
            if transfer_type_text and parse_int(transfer_type_text) != 2:
                raise RuntimeError(f"frame {frame}: non-control row passed filter")

            if urb_type == "'S'":
                if not request_text:
                    # Some enumeration control requests are not decoded into
                    # setup fields. They cannot describe Sony request 0x88.
                    continue
                transaction = {
                    "urb_id": urb_id,
                    "device_address": address,
                    "endpoint": endpoint_text,
                    "submit_frame": frame,
                    "submit_time_epoch": time_text,
                    "request_type": parse_int(request_type_text),
                    "request": parse_int(request_text),
                    "value": parse_int(value_text) if value_text else None,
                    "index": parse_int(index_text) if index_text else None,
                    "length": optional_int(length_text),
                    "output_data": output_data or None,
                    "submit_status": optional_int(status_text),
                    "submit_urb_length": optional_int(urb_length_text),
                    "submit_data_length": optional_int(data_length_text),
                }
                pending[urb_id] = transaction
                continue

            if urb_type != "'C'":
                continue
            transaction = pending.pop(urb_id, None)
            if transaction is None:
                continue
            transaction.update(
                {
                    "complete_frame": frame,
                    "complete_time_epoch": time_text,
                    "duration_us": round(
                        (float(time_text) - float(transaction["submit_time_epoch"]))
                        * 1_000_000
                    ),
                    "status": optional_int(status_text),
                    "actual_length": optional_int(data_length_text),
                    "response_data": response_data or None,
                }
            )
            expected = transaction["length"]
            request_type = transaction["request_type"]
            is_sony_vendor = (
                transaction["request"] == 0x88
                and transaction["request_type"] in (0x40, 0xC0)
            )
            if request_type & 0x80:
                payload = transaction["response_data"]
                if (
                    is_sony_vendor
                    and transaction["status"] == 0
                    and len(payload or "") != expected * 2
                ):
                    raise RuntimeError(
                        f"frames {transaction['submit_frame']}/{frame}: "
                        f"expected {expected} response bytes"
                    )
            else:
                payload = transaction["output_data"]
                if is_sony_vendor and expected and len(payload or "") != expected * 2:
                    raise RuntimeError(
                        f"frame {transaction['submit_frame']}: "
                        f"expected {expected} output bytes"
                    )
            transactions.append(transaction)

    transactions.sort(key=lambda item: item["submit_frame"])
    if device_address is None:
        sony_addresses = {
            item["device_address"]
            for item in transactions
            if item["request"] == 0x88
            and item["request_type"] in (0x40, 0xC0)
        }
        if len(sony_addresses) != 1:
            rendered = ", ".join(str(value) for value in sorted(sony_addresses)) or "none"
            raise RuntimeError(
                f"could not infer one Sony request-0x88 address; found {rendered}"
            )
        selected_address = sony_addresses.pop()
        transactions = [
            item for item in transactions if item["device_address"] == selected_address
        ]
    else:
        selected_address = device_address

    with output.open("x", encoding="utf-8") as stream:
        for transaction in transactions:
            stream.write(json.dumps(transaction, separators=(",", ":")))
            stream.write("\n")
    return {
        "source": str(source),
        "source_sha256": sha256_file(source),
        "device_address": selected_address,
        "transactions": len(transactions),
        "sony_vendor_transactions": sum(
            item["request"] == 0x88 for item in transactions
        ),
        "control_reads": sum(bool(item["request_type"] & 0x80) for item in transactions),
        "control_writes": sum(
            not bool(item["request_type"] & 0x80) for item in transactions
        ),
        "unmatched_submissions": len(pending),
        "observed_addresses": sorted(addresses),
        "output": str(output),
        "output_sha256": sha256_file(output),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("pcap", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--device-address", type=parse_int)
    arguments = parser.parse_args()
    if not arguments.pcap.is_file():
        parser.error(f"pcap does not exist: {arguments.pcap}")
    if arguments.output.exists():
        parser.error(f"output already exists: {arguments.output}")
    summary = extract(arguments.pcap, arguments.output, arguments.device_address)
    print(json.dumps(summary, indent=2))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, RuntimeError, ValueError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1)
