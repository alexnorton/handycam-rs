#!/usr/bin/env python3
"""Switch winxp between ICH6 HDA and XP-era AC'97 virtual audio."""

from __future__ import annotations

import argparse
import difflib
import re

import libvirt


SOUND_PATTERN = re.compile(r"(<sound model=')([^']+)('>)")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("model", choices=("ich6", "ac97"))
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
        changed, count = SOUND_PATTERN.subn(
            rf"\g<1>{args.model}\g<3>", original, count=1
        )
        if count != 1:
            raise RuntimeError("could not locate exactly one sound device")
        diff = difflib.unified_diff(
            original.splitlines(),
            changed.splitlines(),
            fromfile="persistent-current.xml",
            tofile=f"persistent-sound-{args.model}.xml",
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
