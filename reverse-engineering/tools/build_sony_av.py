#!/usr/bin/env python3
"""Build a synchronized A/V sample from an extracted Sony USB capture.

Video records are bounded by endpoint-0x82 descriptor headers and wrapped as
baseline JPEG.  Audio is the standard 16-kHz stereo S16LE endpoint-0x83
stream.  The closest USB-frame observation anchors the streams; video uses a
USB-frame presentation clock because some observed camera timestamps freeze
even while new compressed records continue to arrive.
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import subprocess
import sys
import wave
from pathlib import Path
from typing import Any

from PIL import Image

from analyze_av_sync import assign_unwrapped_usb_frames, load_rows
from decode_sony_records import HEIGHT, WIDTH, jpeg_header, wrap_record


VIDEO_ENDPOINT = "0x82"
AUDIO_ENDPOINT = "0x83"
VIDEO_RATE = 25
NOMINAL_VIDEO_PERIOD_MS = 40
AUDIO_RATE = 16_000
AUDIO_CHANNELS = 2
AUDIO_SAMPLE_WIDTH = 2
AUDIO_BYTES_PER_SECOND = AUDIO_RATE * AUDIO_CHANNELS * AUDIO_SAMPLE_WIDTH


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def find_video_headers(
    rows: list[dict[str, Any]], video_data: bytes
) -> list[dict[str, Any]]:
    headers = []
    for row in rows:
        if row["endpoint"] != VIDEO_ENDPOINT or int(row["length"]) < 8:
            continue
        offset = int(row["data_offset"])
        header = video_data[offset : offset + 8]
        if header[:6] != b"\xff" * 6:
            continue
        headers.append(
            {
                "data_offset": offset,
                "capture_frame": int(row["capture_frame"]),
                "completion_time_epoch": float(row["completion_time_epoch"]),
                "usb_frame": int(row["usb_frame"]),
                "usb_frame_unwrapped": int(row["usb_frame_unwrapped"]),
                "timestamp_ms": ((header[6] & 0x07) << 8) | header[7],
                "flags": header[6] & 0xF8,
            }
        )
    return headers


def group_sessions(
    headers: list[dict[str, Any]], maximum_gap_seconds: float
) -> list[list[dict[str, Any]]]:
    sessions: list[list[dict[str, Any]]] = []
    for header in headers:
        if (
            not sessions
            or header["completion_time_epoch"]
            - sessions[-1][-1]["completion_time_epoch"]
            > maximum_gap_seconds
        ):
            sessions.append([])
        sessions[-1].append(header)
    return sessions


def choose_session(
    sessions: list[list[dict[str, Any]]], requested: int | None
) -> tuple[int, list[dict[str, Any]]]:
    usable = [(index, session) for index, session in enumerate(sessions) if len(session) > 1]
    if not usable:
        raise ValueError("no video session contains a complete bounded record")
    if requested is None:
        return max(usable, key=lambda item: len(item[1]))
    if requested < 0 or requested >= len(sessions):
        raise ValueError(
            f"session {requested} is out of range (capture has {len(sessions)})"
        )
    if len(sessions[requested]) < 2:
        raise ValueError(f"session {requested} has no complete bounded record")
    return requested, sessions[requested]


def write_video_frames(
    output: Path,
    session: list[dict[str, Any]],
    video_data: bytes,
) -> list[dict[str, Any]]:
    jpeg_prefix = jpeg_header()
    frames = []
    for index, (current, following) in enumerate(
        zip(session, session[1:], strict=False)
    ):
        record = video_data[current["data_offset"] : following["data_offset"]]
        jpeg = wrap_record(record, jpeg_prefix)
        with Image.open(io.BytesIO(jpeg)) as image:
            image.load()
            if image.size != (WIDTH, HEIGHT):
                raise ValueError(
                    f"frame {index} decoded as {image.size}, expected {(WIDTH, HEIGHT)}"
                )
        filename = f"frame-{index:04d}.jpg"
        (output / filename).write_bytes(jpeg)
        frames.append(
            {
                "index": index,
                "file": filename,
                "bytes": len(jpeg),
                "sha256": sha256(jpeg),
                "source_timestamp_ms": current["timestamp_ms"],
                "source_usb_frame": current["usb_frame_unwrapped"],
                "presentation_time_ms": current["usb_frame_unwrapped"]
                - session[0]["usb_frame_unwrapped"],
                "duration_ms": following["usb_frame_unwrapped"]
                - current["usb_frame_unwrapped"],
            }
        )
    return frames


def audio_anchor(
    rows: list[dict[str, Any]], first_video: dict[str, Any]
) -> dict[str, Any]:
    audio_rows = [
        row
        for row in rows
        if row["endpoint"] == AUDIO_ENDPOINT and int(row["length"]) > 0
    ]
    if not audio_rows:
        raise ValueError("capture contains no endpoint-0x83 audio")
    return min(
        audio_rows,
        key=lambda row: (
            abs(
                int(row["usb_frame_unwrapped"])
                - first_video["usb_frame_unwrapped"]
            ),
            abs(
                float(row["completion_time_epoch"])
                - first_video["completion_time_epoch"]
            ),
        ),
    )


def write_audio(
    output: Path,
    audio_data: bytes,
    anchor: dict[str, Any],
    duration_ms: int,
) -> dict[str, Any]:
    wanted_bytes = duration_ms * AUDIO_BYTES_PER_SECOND // 1_000
    start = int(anchor["data_offset"])
    pcm = audio_data[start : start + wanted_bytes]
    if len(pcm) != wanted_bytes:
        raise ValueError(
            f"only {len(pcm)} audio bytes remain, need {wanted_bytes}"
        )
    destination = output / "audio.wav"
    with wave.open(str(destination), "wb") as stream:
        stream.setnchannels(AUDIO_CHANNELS)
        stream.setsampwidth(AUDIO_SAMPLE_WIDTH)
        stream.setframerate(AUDIO_RATE)
        stream.writeframes(pcm)
    return {
        "file": destination.name,
        "source_data_offset": start,
        "source_usb_frame": int(anchor["usb_frame_unwrapped"]),
        "source_capture_frame": int(anchor["capture_frame"]),
        "pcm_bytes": len(pcm),
        "sample_frames": len(pcm) // (AUDIO_CHANNELS * AUDIO_SAMPLE_WIDTH),
        "duration_ms": round(len(pcm) / AUDIO_BYTES_PER_SECOND * 1_000),
        "sha256_pcm": sha256(pcm),
    }


def write_concat_file(output: Path, frames: list[dict[str, Any]]) -> Path:
    destination = output / "frames.ffconcat"
    with destination.open("w", encoding="utf-8") as stream:
        stream.write("ffconcat version 1.0\n")
        for frame in frames:
            stream.write(f"file '{frame['file']}'\n")
            stream.write(f"duration {frame['duration_ms'] / 1_000:.6f}\n")
        # FFmpeg's concat demuxer applies the final duration only when another
        # file follows. Repeating the last file supplies an end timestamp;
        # -shortest trims the duplicate against the exactly matching audio.
        stream.write(f"file '{frames[-1]['file']}'\n")
    return destination


def mux(output: Path, frames: list[dict[str, Any]], ffmpeg: str) -> str:
    destination = output / "capture.mkv"
    concat = write_concat_file(output, frames)
    command = [
        ffmpeg,
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "concat",
        "-safe",
        "0",
        "-i",
        str(concat),
        "-i",
        str(output / "audio.wav"),
        "-map",
        "0:v:0",
        "-map",
        "1:a:0",
        "-c:v",
        "copy",
        "-c:a",
        "pcm_s16le",
        "-fps_mode",
        "passthrough",
        "-shortest",
        str(destination),
    ]
    subprocess.run(command, check=True)
    return destination.name


def timestamp_deltas(frames: list[dict[str, Any]]) -> dict[str, int]:
    counts: dict[str, int] = {}
    for previous, current in zip(frames, frames[1:], strict=False):
        delta = (
            current["source_timestamp_ms"] - previous["source_timestamp_ms"]
        ) % 2048
        key = str(delta)
        counts[key] = counts.get(key, 0) + 1
    return dict(sorted(counts.items(), key=lambda item: int(item[0])))


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("capture", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument(
        "--session",
        type=int,
        help="zero-based video session; default is the one with most headers",
    )
    parser.add_argument(
        "--session-gap",
        type=float,
        default=0.5,
        help="seconds between video headers that starts a new session",
    )
    parser.add_argument("--ffmpeg", default="ffmpeg")
    parser.add_argument("--no-mux", action="store_true")
    return parser.parse_args()


def main() -> int:
    arguments = parse_args()
    capture = arguments.capture.resolve()
    output = arguments.output.resolve()
    try:
        if output.exists():
            raise ValueError(f"output already exists: {output}")
        rows = load_rows(capture / "iso-packets.jsonl")
        assign_unwrapped_usb_frames(rows)
        video_data = (capture / "ep82.bin").read_bytes()
        audio_data = (capture / "ep83.bin").read_bytes()
        headers = find_video_headers(rows, video_data)
        sessions = group_sessions(headers, arguments.session_gap)
        session_index, session = choose_session(sessions, arguments.session)

        output.mkdir(parents=True)
        frames = write_video_frames(output, session, video_data)
        anchor = audio_anchor(rows, session[0])
        duration_ms = sum(frame["duration_ms"] for frame in frames)
        audio = write_audio(output, audio_data, anchor, duration_ms)
        muxed_file = (
            None
            if arguments.no_mux
            else mux(output, frames, arguments.ffmpeg)
        )
        manifest = {
            "schema": 1,
            "capture": str(capture),
            "session": {
                "selected_index": session_index,
                "available_header_counts": [len(item) for item in sessions],
                "source_first_capture_frame": session[0]["capture_frame"],
                "source_last_capture_frame": session[-1]["capture_frame"],
                "header_count": len(session),
                "complete_video_frames": len(frames),
            },
            "synchronization": {
                "method": "nearest unwrapped 1-ms USB frame",
                "video_anchor_usb_frame": session[0]["usb_frame_unwrapped"],
                "audio_anchor_usb_frame": audio["source_usb_frame"],
                "anchor_difference_ms": audio["source_usb_frame"]
                - session[0]["usb_frame_unwrapped"],
                "video_clock": "unwrapped 1-ms USB frame",
                "source_timestamp_delta_histogram": timestamp_deltas(frames),
            },
            "video": {
                "width": WIDTH,
                "height": HEIGHT,
                "nominal_frame_rate": VIDEO_RATE,
                "duration_ms": duration_ms,
                "frames": frames,
            },
            "audio": audio,
            "muxed_file": muxed_file,
        }
        with (output / "manifest.json").open("w", encoding="utf-8") as stream:
            json.dump(manifest, stream, indent=2)
            stream.write("\n")
        print(
            json.dumps(
                {
                    "output": str(output),
                    "session": manifest["session"],
                    "synchronization": manifest["synchronization"],
                    "audio": audio,
                    "muxed_file": muxed_file,
                },
                indent=2,
            )
        )
        return 0
    except (
        OSError,
        ValueError,
        KeyError,
        json.JSONDecodeError,
        subprocess.CalledProcessError,
    ) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
