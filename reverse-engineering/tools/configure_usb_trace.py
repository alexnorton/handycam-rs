#!/usr/bin/env python3
"""Configure creation-time QEMU tracing/tuning for the winxp camera."""

from __future__ import annotations

import argparse
import difflib
import re
from pathlib import Path

import libvirt


QEMU_NAMESPACE = "http://libvirt.org/schemas/domain/qemu/1.0"
OVERRIDE_PATTERN = re.compile(
    r"\n  <qemu:override>.*?</qemu:override>\n", re.DOTALL
)


def add_namespace(xml: str) -> str:
    root_end = xml.index(">")
    root = xml[:root_end]
    if "xmlns:qemu=" not in root:
        root += f" xmlns:qemu='{QEMU_NAMESPACE}'"
    return root + xml[root_end:]


def remove_override(xml: str) -> str:
    return OVERRIDE_PATTERN.sub("\n", xml, count=1)


def override_xml(
    pcap: Path | None, isobufs: int | None, isobsize: int | None
) -> str:
    properties = []
    if pcap is not None:
        properties.append(
            f"        <qemu:property name='pcap' type='string' value='{pcap}'/>"
        )
    if isobufs is not None:
        properties.append(
            "        "
            f"<qemu:property name='isobufs' type='unsigned' value='{isobufs}'/>"
        )
    if isobsize is not None:
        properties.append(
            "        "
            f"<qemu:property name='isobsize' type='unsigned' value='{isobsize}'/>"
        )
    joined = "\n".join(properties)
    return (
        "  <qemu:override>\n"
        "    <qemu:device alias='ua-camera'>\n"
        "      <qemu:frontend>\n"
        f"{joined}\n"
        "      </qemu:frontend>\n"
        "    </qemu:device>\n"
        "  </qemu:override>"
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("action", choices=("enable", "disable"))
    parser.add_argument("--vm", default="winxp")
    parser.add_argument("--pcap", type=Path, default=Path("/tmp/handycam-usb.pcap"))
    parser.add_argument(
        "--no-pcap",
        action="store_true",
        help="apply isochronous tuning without QEMU's pcap property",
    )
    parser.add_argument("--isobufs", type=int)
    parser.add_argument("--isobsize", type=int)
    parser.add_argument(
        "--apply", action="store_true", help="define the changed persistent XML"
    )
    args = parser.parse_args()

    pcap = None if args.no_pcap else args.pcap.resolve()
    if args.action == "enable" and pcap is not None and pcap.parent != Path("/tmp"):
        parser.error("--pcap must be directly under /tmp for QEMU sandbox access")
    if (
        args.action == "enable"
        and pcap is None
        and args.isobufs is None
        and args.isobsize is None
    ):
        parser.error("--no-pcap requires --isobufs and/or --isobsize")
    for option in ("isobufs", "isobsize"):
        value = getattr(args, option)
        if value is not None and value <= 0:
            parser.error(f"--{option} must be positive")

    connection = libvirt.open("qemu:///system")
    if connection is None:
        raise RuntimeError("could not connect to qemu:///system")
    try:
        domain = connection.lookupByName(args.vm)
        original = domain.XMLDesc(libvirt.VIR_DOMAIN_XML_INACTIVE)
        changed = remove_override(original)
        if args.action == "enable":
            if "alias name='ua-camera'" not in changed:
                raise RuntimeError(
                    "persistent Sony hostdev must have alias 'ua-camera'"
                )
            changed = add_namespace(changed)
            block = override_xml(pcap, args.isobufs, args.isobsize)
            changed = changed.replace("\n</domain>", f"\n{block}\n</domain>", 1)

        diff = difflib.unified_diff(
            original.splitlines(),
            changed.splitlines(),
            fromfile="persistent-current.xml",
            tofile=f"persistent-{args.action}.xml",
            lineterm="",
        )
        print("\n".join(diff) or "No change.")
        if args.apply and changed != original:
            connection.defineXML(changed)
            print(f"Persistent {args.vm} definition updated.")
            print("The change takes effect after a full power off and start.")
    finally:
        connection.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
