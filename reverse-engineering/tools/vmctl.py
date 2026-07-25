#!/usr/bin/env python3
"""Small libvirt/QEMU UI-control helper for the winxp research VM."""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
import time
from pathlib import Path

import libvirt
import libvirt_qemu


SHIFTED = {
    "!": "1",
    '"': "apostrophe",
    "#": "3",
    "$": "4",
    "%": "5",
    "&": "7",
    "(": "9",
    ")": "0",
    "*": "8",
    "+": "equal",
    ":": "semicolon",
    "<": "comma",
    ">": "dot",
    "?": "slash",
    "@": "2",
    "^": "6",
    "_": "minus",
    "{": "bracket_left",
    "|": "backslash",
    "}": "bracket_right",
    "~": "grave_accent",
}

PLAIN = {
    " ": "spc",
    "\n": "ret",
    "\r": "ret",
    "\t": "tab",
    "'": "apostrophe",
    ",": "comma",
    "-": "minus",
    ".": "dot",
    "/": "slash",
    ";": "semicolon",
    "=": "equal",
    "[": "bracket_left",
    "\\": "backslash",
    "]": "bracket_right",
    "`": "grave_accent",
}


def virsh(*args: str, capture: bool = False) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["virsh", *args],
        check=True,
        text=True,
        capture_output=capture,
    )


def command_output(*args: str) -> str:
    result = subprocess.run(
        list(args),
        check=False,
        text=True,
        capture_output=True,
    )
    return result.stdout.strip()


def hmp(vm: str, command: str) -> None:
    virsh("qemu-monitor-command", vm, "--hmp", command)


def qmp_text(vm: str, commands: list[dict[str, object]], delay: float) -> None:
    connection = libvirt.open("qemu:///system")
    if connection is None:
        raise RuntimeError("could not connect to qemu:///system")
    try:
        domain = connection.lookupByName(vm)
        for command in commands:
            result = libvirt_qemu.qemuMonitorCommand(
                domain, json.dumps(command), 0
            )
            reply = json.loads(result)
            if "error" in reply:
                raise RuntimeError(str(reply["error"]))
            if delay:
                time.sleep(delay)
    finally:
        connection.close()


def qemu_key(character: str) -> str:
    if character in SHIFTED:
        return f"shift-{SHIFTED[character]}"
    if character in PLAIN:
        return PLAIN[character]
    if character.isalpha():
        return f"shift-{character.lower()}" if character.isupper() else character
    if character.isdigit():
        return character
    raise ValueError(f"unsupported character: {character!r}")


def key_event(qcode: str, down: bool) -> dict[str, object]:
    return {
        "type": "key",
        "data": {
            "down": down,
            "key": {"type": "qcode", "data": qcode},
        },
    }


def absolute_pointer_events(
    x: int, y: int, width: int, height: int
) -> list[dict[str, object]]:
    if width <= 1 or height <= 1:
        raise ValueError("screen dimensions must be greater than one")
    if not 0 <= x < width or not 0 <= y < height:
        raise ValueError(
            f"point ({x}, {y}) is outside {width}x{height} guest screen"
        )
    maximum = 0x7FFF
    return [
        {
            "type": "abs",
            "data": {
                "axis": "x",
                "value": round(x * maximum / (width - 1)),
            },
        },
        {
            "type": "abs",
            "data": {
                "axis": "y",
                "value": round(y * maximum / (height - 1)),
            },
        },
    ]


def pointer_button_event(button: str, down: bool) -> dict[str, object]:
    return {
        "type": "btn",
        "data": {
            "down": down,
            "button": button,
        },
    }


def click(vm: str, button: str, delay: float = 0.1) -> None:
    """Send a paced button press through QMP's active input device."""
    commands = [
        {
            "execute": "input-send-event",
            "arguments": {
                "events": [pointer_button_event(button, True)]
            },
        },
        {
            "execute": "input-send-event",
            "arguments": {
                "events": [pointer_button_event(button, False)]
            },
        },
    ]
    qmp_text(vm, commands, delay)


def point(
    vm: str,
    x: int,
    y: int,
    width: int,
    height: int,
    click_button: str | None,
) -> None:
    commands = [
        {
            "execute": "input-send-event",
            "arguments": {
                "events": absolute_pointer_events(x, y, width, height)
            },
        }
    ]
    if click_button is not None:
        # XP can process the tablet button before the preceding absolute
        # position when both are sent in one QMP event batch. Use separate,
        # paced input commands so the click lands at the visible cursor.
        commands.extend(
            [
                {
                    "execute": "input-send-event",
                    "arguments": {
                        "events": [
                            pointer_button_event(click_button, True)
                        ]
                    },
                },
                {
                    "execute": "input-send-event",
                    "arguments": {
                        "events": [
                            pointer_button_event(click_button, False)
                        ]
                    },
                },
            ]
        )
    qmp_text(vm, commands, 0.1 if click_button is not None else 0)


def type_text(vm: str, value: str, delay: float) -> None:
    commands: list[dict[str, object]] = []
    for character in value:
        key = qemu_key(character)
        if key.startswith("shift-"):
            qcode = key.removeprefix("shift-")
            events = [
                key_event("shift", True),
                key_event(qcode, True),
                key_event(qcode, False),
                key_event("shift", False),
            ]
        else:
            events = [key_event(key, True), key_event(key, False)]
        commands.append(
            {"execute": "input-send-event", "arguments": {"events": events}}
        )

    # Keep one libvirt connection open and pace individual key events. Sending
    # an entire string as one QMP event array can overflow XP's PS/2 queue.
    qmp_text(vm, commands, delay)


def camera_state(vm: str) -> tuple[bool, bool]:
    host_result = subprocess.run(
        ["lsusb", "-d", "054c:00c0"],
        check=False,
        text=True,
        capture_output=True,
    )
    qtree = command_output(
        "virsh", "qemu-monitor-command", vm, "--hmp", "info qtree"
    )
    # An optional-but-absent libvirt hostdev remains as a QEMU placeholder
    # with vendorid/productid zero. Check the resolved QEMU properties rather
    # than the live XML declaration.
    guest_attached = any(
        'hostdevice = "/dev/bus/usb/' in block
        for block in qtree.split("dev: usb-host")[1:]
    )
    return host_result.returncode == 0, guest_attached


def show_status(vm: str) -> None:
    print(f"VM: {vm} ({command_output('virsh', 'domstate', vm)})")
    for display_type in ("spice", "vnc"):
        display = command_output("virsh", "domdisplay", vm, "--type", display_type)
        if display:
            print(f"{display_type.upper()}: {display}")
    print("Network:")
    print(command_output("virsh", "domifaddr", vm, "--source", "lease"))
    host_present, guest_attached = camera_state(vm)
    print(f"Camera on host: {'yes' if host_present else 'no'}")
    print(f"Camera attached to VM: {'yes' if guest_attached else 'no'}")


def attach_camera(vm: str) -> None:
    host_present, guest_attached = camera_state(vm)
    if guest_attached:
        print("Sony 054c:00c0 is already attached to the VM.")
        return
    if not host_present:
        raise RuntimeError(
            "Sony 054c:00c0 is not present on the host; turn on USB-stream mode first"
        )
    camera_xml = Path(__file__).resolve().parent / "libvirt" / "camera.xml"
    virsh("attach-device", vm, str(camera_xml), "--live")
    print("Sony 054c:00c0 attached to the running VM.")


def detach_camera(vm: str) -> None:
    _, guest_attached = camera_state(vm)
    if not guest_attached:
        print("Sony 054c:00c0 is not attached to the VM.")
        return
    camera_xml = Path(__file__).resolve().parent / "libvirt" / "camera.xml"
    virsh("detach-device", vm, str(camera_xml), "--live")
    print("Sony 054c:00c0 detached from the running VM.")
    print(
        "This Handycam may leave the host USB bus after a live detach; "
        "reconnect or power-cycle it if it does not re-enumerate."
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--vm", default="winxp")
    subparsers = parser.add_subparsers(dest="command", required=True)

    key_parser = subparsers.add_parser("key", help="send a QEMU key chord")
    key_parser.add_argument("keys", help="for example: meta_l-r or ctrl-alt-delete")

    text_parser = subparsers.add_parser("text", help="type text into the guest")
    text_parser.add_argument("value")
    text_parser.add_argument("--delay", type=float, default=0.02)
    text_parser.add_argument("--enter", action="store_true")

    move_parser = subparsers.add_parser("move", help="move the relative guest mouse")
    move_parser.add_argument("dx", type=int)
    move_parser.add_argument("dy", type=int)

    point_parser = subparsers.add_parser(
        "point", help="move the absolute guest tablet pointer"
    )
    point_parser.add_argument("x", type=int)
    point_parser.add_argument("y", type=int)
    point_parser.add_argument("--width", type=int, default=800)
    point_parser.add_argument("--height", type=int, default=600)
    point_parser.add_argument(
        "--click", choices=("left", "middle", "right")
    )

    click_parser = subparsers.add_parser("click", help="click a guest mouse button")
    click_parser.add_argument(
        "--button", choices=("left", "middle", "right"), default="left"
    )

    shot_parser = subparsers.add_parser("screenshot", help="save a guest screenshot")
    shot_parser.add_argument("path", type=Path)

    subparsers.add_parser("status", help="show VM control endpoints and camera state")

    camera_parser = subparsers.add_parser(
        "camera", help="inspect, attach, or live-detach the camera"
    )
    camera_parser.add_argument("action", choices=("status", "attach", "detach"))

    args = parser.parse_args()
    if args.command == "key":
        hmp(args.vm, f"sendkey {args.keys}")
    elif args.command == "text":
        type_text(args.vm, args.value, args.delay)
        if args.enter:
            hmp(args.vm, "sendkey ret")
    elif args.command == "move":
        hmp(args.vm, f"mouse_move {args.dx} {args.dy}")
    elif args.command == "point":
        point(
            args.vm,
            args.x,
            args.y,
            args.width,
            args.height,
            args.click,
        )
    elif args.command == "click":
        click(args.vm, args.button)
    elif args.command == "screenshot":
        output = args.path.expanduser().resolve()
        output.parent.mkdir(parents=True, exist_ok=True)
        virsh("screenshot", args.vm, str(output))
        print(output)
    elif args.command == "status":
        show_status(args.vm)
    elif args.command == "camera":
        if args.action == "status":
            host_present, guest_attached = camera_state(args.vm)
            print(f"Camera on host: {'yes' if host_present else 'no'}")
            print(f"Camera attached to VM: {'yes' if guest_attached else 'no'}")
        elif args.action == "attach":
            attach_camera(args.vm)
        else:
            detach_camera(args.vm)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (libvirt.libvirtError, RuntimeError, subprocess.CalledProcessError, ValueError) as error:
        print(error, file=sys.stderr)
        raise SystemExit(1)
