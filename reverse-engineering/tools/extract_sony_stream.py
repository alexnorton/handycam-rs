#!/usr/bin/env python3
"""Extract Sony isochronous USB packets without losing packet boundaries.

Linux usbmon stores one completion URB containing a group of ISO descriptors.
This tool writes each endpoint's payload to a binary file and records the
offset and metadata of every ISO descriptor in JSON Lines.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import subprocess
import sys
import tempfile
from collections import Counter
from pathlib import Path
from typing import BinaryIO, Iterator


SONY_VENDOR_ID = 0x054C
SONY_PRODUCT_ID = 0x00C0
FIELD_SEPARATOR = "\t"
TSHARK_FIELDS = (
    "frame.number",
    "frame.time_epoch",
    "usb.device_address",
    "usb.endpoint_address",
    "usb.urb_id",
    "usb.urb_status",
    "usb.urb_len",
    "usb.start_frame",
    "usb.iso.error_count",
    "usb.iso.numdesc",
    "usb.iso.iso_len",
    "usb.iso.iso_status",
    "usb.iso.data",
)


def run_tshark(*arguments: str) -> subprocess.CompletedProcess[str]:
    try:
        return subprocess.run(
            ("tshark", *arguments),
            check=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
    except FileNotFoundError:
        raise RuntimeError("tshark was not found in PATH") from None
    except subprocess.CalledProcessError as error:
        detail = error.stderr.strip() or f"exit status {error.returncode}"
        raise RuntimeError(f"tshark failed: {detail}") from None


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def parse_int(value: str) -> int:
    return int(value, 0)


def parse_int_list(value: str) -> list[int]:
    if not value:
        return []
    return [parse_int(item) for item in value.split(",")]


def first_int(value: str) -> int:
    """Return the first occurrence of a scalar duplicated by Wireshark."""
    values = parse_int_list(value)
    if not values:
        raise ValueError("missing integer field")
    return values[0]


def discover_device_address(pcap: Path) -> int:
    display_filter = (
        f"usb.idVendor == 0x{SONY_VENDOR_ID:04x} && "
        f"usb.idProduct == 0x{SONY_PRODUCT_ID:04x}"
    )
    result = run_tshark(
        "-r",
        str(pcap),
        "-Y",
        display_filter,
        "-T",
        "fields",
        "-e",
        "usb.device_address",
    )
    addresses = {
        parse_int(line.strip())
        for line in result.stdout.splitlines()
        if line.strip() and parse_int(line.strip()) != 0
    }
    if not addresses:
        raise RuntimeError(
            "could not find a nonzero 054c:00c0 device address; "
            "pass --device-address"
        )
    if len(addresses) != 1:
        rendered = ", ".join(str(address) for address in sorted(addresses))
        raise RuntimeError(
            f"capture contains Sony devices at addresses {rendered}; "
            "pass --device-address"
        )
    return addresses.pop()


def tshark_rows(
    pcap: Path, device_address: int, endpoints: tuple[int, ...]
) -> Iterator[list[str]]:
    endpoint_filter = " || ".join(
        f"usb.endpoint_address == 0x{endpoint:02x}" for endpoint in endpoints
    )
    display_filter = (
        f"usb.device_address == {device_address} && "
        "usb.urb_type == 0x43 && "
        "usb.urb_status == 0 && "
        f"({endpoint_filter})"
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
        "occurrence=a",
        "-E",
        f"separator={FIELD_SEPARATOR}",
    ]
    for field in TSHARK_FIELDS:
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
        row = line.rstrip("\n").split(FIELD_SEPARATOR)
        if len(row) != len(TSHARK_FIELDS):
            process.kill()
            raise RuntimeError(
                f"unexpected tshark output at row {line_number}: "
                f"got {len(row)} fields, expected {len(TSHARK_FIELDS)}"
            )
        yield row
    stderr = process.stderr.read().strip()
    return_code = process.wait()
    if return_code:
        raise RuntimeError(
            f"tshark failed with exit status {return_code}: {stderr}"
        )


def extract(
    pcap: Path,
    output: Path,
    device_address: int,
    endpoints: tuple[int, ...],
) -> dict[str, object]:
    output.mkdir(parents=True, exist_ok=False)
    metadata_path = output / "iso-packets.jsonl"
    endpoint_paths = {
        endpoint: output / f"ep{endpoint:02x}.bin" for endpoint in endpoints
    }
    endpoint_files: dict[int, BinaryIO] = {
        endpoint: path.open("wb") for endpoint, path in endpoint_paths.items()
    }
    endpoint_offsets = {endpoint: 0 for endpoint in endpoints}
    descriptor_counts: Counter[int] = Counter()
    payload_counts: Counter[int] = Counter()
    urb_counts: Counter[int] = Counter()
    urb_status_counts: Counter[tuple[int, int]] = Counter()
    descriptor_status_counts: Counter[tuple[int, int]] = Counter()

    try:
        with metadata_path.open("w", encoding="utf-8") as metadata:
            for row in tshark_rows(pcap, device_address, endpoints):
                (
                    capture_frame_text,
                    completion_time,
                    row_device_text,
                    endpoint_text,
                    urb_id,
                    urb_status_text,
                    urb_length_text,
                    start_frame_text,
                    error_count_text,
                    descriptor_count_text,
                    lengths_text,
                    statuses_text,
                    data_text,
                ) = row
                capture_frame = parse_int(capture_frame_text)
                row_device = parse_int(row_device_text)
                endpoint = parse_int(endpoint_text)
                urb_status = parse_int(urb_status_text)
                urb_length = parse_int(urb_length_text)
                start_frame = parse_int(start_frame_text)
                error_count = parse_int(error_count_text)
                descriptor_count = first_int(descriptor_count_text)
                lengths = parse_int_list(lengths_text)
                statuses = parse_int_list(statuses_text)
                chunks = (
                    [bytes.fromhex(chunk) for chunk in data_text.split(",")]
                    if data_text
                    else []
                )

                if row_device != device_address or endpoint not in endpoint_files:
                    raise RuntimeError(
                        f"tshark filter mismatch at capture frame {capture_frame}"
                    )
                if len(lengths) != descriptor_count:
                    raise RuntimeError(
                        f"frame {capture_frame}: {len(lengths)} ISO lengths for "
                        f"{descriptor_count} descriptors"
                    )
                if len(statuses) != descriptor_count:
                    raise RuntimeError(
                        f"frame {capture_frame}: {len(statuses)} ISO statuses "
                        f"for {descriptor_count} descriptors"
                    )
                nonempty_lengths = [length for length in lengths if length]
                if len(chunks) != len(nonempty_lengths):
                    raise RuntimeError(
                        f"frame {capture_frame}: {len(chunks)} data chunks for "
                        f"{len(nonempty_lengths)} nonempty descriptors"
                    )
                if sum(lengths) != urb_length:
                    raise RuntimeError(
                        f"frame {capture_frame}: ISO lengths sum to "
                        f"{sum(lengths)}, URB length is {urb_length}"
                    )

                urb_counts[endpoint] += 1
                urb_status_counts[(endpoint, urb_status)] += 1
                chunk_index = 0
                for descriptor_index, (length, status) in enumerate(
                    zip(lengths, statuses)
                ):
                    data_offset = None
                    if length:
                        chunk = chunks[chunk_index]
                        chunk_index += 1
                        if len(chunk) != length:
                            raise RuntimeError(
                                f"frame {capture_frame}, descriptor "
                                f"{descriptor_index}: declared {length} bytes, "
                                f"decoded {len(chunk)}"
                            )
                        data_offset = endpoint_offsets[endpoint]
                        endpoint_files[endpoint].write(chunk)
                        endpoint_offsets[endpoint] += length
                        payload_counts[endpoint] += 1

                    descriptor_counts[endpoint] += 1
                    descriptor_status_counts[(endpoint, status)] += 1
                    record = {
                        "capture_frame": capture_frame,
                        "completion_time_epoch": completion_time,
                        "device_address": device_address,
                        "endpoint": f"0x{endpoint:02x}",
                        "urb_id": urb_id,
                        "urb_status": urb_status,
                        "urb_length": urb_length,
                        "urb_start_frame": start_frame,
                        "urb_error_count": error_count,
                        "urb_descriptor_count": descriptor_count,
                        "descriptor_index": descriptor_index,
                        "usb_frame": (start_frame + descriptor_index) & 0x7FF,
                        "descriptor_status": status,
                        "length": length,
                        "data_file": endpoint_paths[endpoint].name,
                        "data_offset": data_offset,
                    }
                    metadata.write(
                        json.dumps(record, separators=(",", ":")) + "\n"
                    )
    finally:
        for endpoint_file in endpoint_files.values():
            endpoint_file.close()

    endpoint_summary = {}
    for endpoint in endpoints:
        path = endpoint_paths[endpoint]
        endpoint_summary[f"0x{endpoint:02x}"] = {
            "file": path.name,
            "sha256": sha256_file(path),
            "bytes": endpoint_offsets[endpoint],
            "urbs": urb_counts[endpoint],
            "descriptors": descriptor_counts[endpoint],
            "nonempty_descriptors": payload_counts[endpoint],
            "urb_status_counts": {
                str(status): count
                for (item_endpoint, status), count in sorted(
                    urb_status_counts.items()
                )
                if item_endpoint == endpoint
            },
            "descriptor_status_counts": {
                str(status): count
                for (item_endpoint, status), count in sorted(
                    descriptor_status_counts.items()
                )
                if item_endpoint == endpoint
            },
        }
    summary: dict[str, object] = {
        "source": str(pcap),
        "source_sha256": sha256_file(pcap),
        "device": {
            "vendor_id": f"0x{SONY_VENDOR_ID:04x}",
            "product_id": f"0x{SONY_PRODUCT_ID:04x}",
            "address": device_address,
        },
        "metadata_file": metadata_path.name,
        "metadata_sha256": sha256_file(metadata_path),
        "endpoints": endpoint_summary,
    }
    with (output / "summary.json").open("w", encoding="utf-8") as summary_file:
        json.dump(summary, summary_file, indent=2)
        summary_file.write("\n")
    return summary


def parse_endpoint(value: str) -> int:
    endpoint = parse_int(value)
    if endpoint < 0 or endpoint > 0xFF:
        raise argparse.ArgumentTypeError("endpoint must be between 0 and 0xff")
    return endpoint


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("pcap", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--device-address", type=parse_int)
    parser.add_argument(
        "--endpoint",
        type=parse_endpoint,
        action="append",
        dest="endpoints",
        help="endpoint to extract; repeat as needed (default: 0x81 and 0x82)",
    )
    parser.add_argument(
        "--no-stage",
        action="store_true",
        help="read the pcap in place instead of staging it under /tmp",
    )
    args = parser.parse_args()

    source = args.pcap.resolve()
    output = args.output.resolve()
    endpoints = tuple(dict.fromkeys(args.endpoints or (0x81, 0x82)))
    if not source.is_file():
        parser.error(f"pcap does not exist: {source}")
    if output.exists():
        parser.error(f"output path already exists: {output}")

    staged_context: tempfile.TemporaryDirectory[str] | None = None
    analysis_source = source
    if not args.no_stage:
        staged_context = tempfile.TemporaryDirectory(
            prefix="handycam-extract-", dir="/tmp"
        )
        analysis_source = Path(staged_context.name) / source.name
        shutil.copyfile(source, analysis_source)

    try:
        device_address = (
            args.device_address
            if args.device_address is not None
            else discover_device_address(analysis_source)
        )
        summary = extract(
            analysis_source, output, device_address, endpoints
        )
        # Record the user's source path/hash, not the temporary staged filename.
        summary["source"] = str(source)
        summary["source_sha256"] = sha256_file(source)
        with (output / "summary.json").open(
            "w", encoding="utf-8"
        ) as summary_file:
            json.dump(summary, summary_file, indent=2)
            summary_file.write("\n")
    except Exception:
        if output.exists():
            print(
                f"Extraction failed; partial output retained in {output}",
                file=sys.stderr,
            )
        raise
    finally:
        if staged_context is not None:
            staged_context.cleanup()

    print(json.dumps(summary, indent=2))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (RuntimeError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(1)
