#!/usr/bin/env python3
"""Move the Handycam between the default ICH9 and a test USB controller."""

from __future__ import annotations

import argparse
import difflib
import re

import libvirt


TEST_CONTROLLER_PATTERN = re.compile(
    r"\n    <controller type='usb' index='1' "
    r"model='(?:piix3-uhci|pci-ohci)'"
    r"(?:/>|>.*?</controller>)",
    re.DOTALL,
)
CONTROLLER_MODELS = {
    "piix3": "piix3-uhci",
    "ohci": "pci-ohci",
}
CAMERA_ADDRESS_PATTERN = re.compile(
    r"(<hostdev mode='subsystem' type='usb' managed='yes'>"
    r".*?<alias name='ua-camera'/>"
    r".*?<address type='usb' bus=')(\d+)(' port=')(\d+)('/>"
    r".*?</hostdev>)",
    re.DOTALL,
)


def set_camera_bus(xml: str, bus: int) -> str:
    match = CAMERA_ADDRESS_PATTERN.search(xml)
    if match is None:
        raise RuntimeError("could not locate ua-camera USB address")
    return (
        xml[: match.start()]
        + match.group(1)
        + str(bus)
        + match.group(3)
        + "1"
        + match.group(5)
        + xml[match.end() :]
    )


def set_mode(xml: str, mode: str) -> str:
    changed = TEST_CONTROLLER_PATTERN.sub("", xml, count=1)
    if mode in CONTROLLER_MODELS:
        marker = "    <controller type='pci' index='0' model='pci-root'/>"
        if marker not in changed:
            raise RuntimeError("could not locate PCI root controller")
        controller_xml = (
            "    <controller type='usb' index='1' "
            f"model='{CONTROLLER_MODELS[mode]}'/>"
        )
        changed = changed.replace(marker, f"{controller_xml}\n{marker}", 1)
        return set_camera_bus(changed, 1)
    return set_camera_bus(changed, 0)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=("piix3", "ohci", "ich9"))
    parser.add_argument("--vm", default="winxp")
    parser.add_argument(
        "--apply", action="store_true", help="define the changed persistent XML"
    )
    args = parser.parse_args()

    connection = libvirt.open("qemu:///system")
    if connection is None:
        raise RuntimeError("could not connect to qemu:///system")
    try:
        domain = connection.lookupByName(args.vm)
        original = domain.XMLDesc(libvirt.VIR_DOMAIN_XML_INACTIVE)
        changed = set_mode(original, args.mode)
        diff = difflib.unified_diff(
            original.splitlines(),
            changed.splitlines(),
            fromfile="persistent-current.xml",
            tofile=f"persistent-{args.mode}.xml",
            lineterm="",
        )
        print("\n".join(diff) or "No change.")
        if args.apply and changed != original:
            connection.defineXMLFlags(
                changed, libvirt.VIR_DOMAIN_DEFINE_VALIDATE
            )
            print(f"Persistent {args.vm} definition updated.")
            print("The change takes effect after a full power off and start.")
    finally:
        connection.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
