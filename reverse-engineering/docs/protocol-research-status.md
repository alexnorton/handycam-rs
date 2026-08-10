# Protocol research status

This records which reverse-engineering goals are implemented and which
protocol questions remain open. The known-good `record-mode-init.tsv` remains
the regression oracle for initialization changes.

## 1. Semantic initialization

The first pass is now implemented in `analyze_sony_driver.py`. It reads the
PE sections and CodeView symbol map directly, extracts named register/value
tables, and finds them in the captured plan. We can already label:

1. power-off;
2. audio-on;
3. the 21-write `DevmanInit` block;
4. power-on;
5. JPEG quantization-table uploads;
6. the scale-factor write; and
7. command-mailbox writes.

`annotate_sony_init.py` now labels all 223 operations using exact,
high-confidence, inferred, composite, or unresolved evidence. Disassembly of
`ToDeviceCaptureStart` supplies the complete routine call order. The startup
mailbox scan also has a real completion condition: poll status byte 0 until
it equals the command's final token.

The first semantic replacement is implemented and live-validated. Four
startup scan commands now poll `status[0]` for their actual tokens instead of
replaying 57 captured reads. One run acknowledged tokens `0x71`, `0x81`,
`0x91`, and `0xa1` after 8, 4, 6, and 5 polls. Initialization fell from
approximately 2.66 seconds to 1.07 seconds, and repeated quality runs plus a
32-second soak remained clean.

Further initialization reduction should replace one additional phase at a
time with generated semantic operations. Validate each replacement against:

- the exact request bytes;
- the original ordering and required delays;
- five-second direct capture;
- 25 headers per second and 40-ms normal timestamp cadence; and
- clean stop, reconnect, and a longer soak.

Only after a phase has a named generator and live evidence should it be
removed from the fallback replay. Literal replay remains available as the
control implementation while the remaining phases are promoted.

## 2. Playback transport controls

The control path is structurally known: `CustomPropCommandWrite` writes a
zeroed 64-byte block to index `0x0300`, with a big-endian command word in its
last four bytes. The Rust core now has a tested builder for this exact
mailbox.

Static analysis of `CapturingTool.exe`, `USB01.dll`, `S2UCapture.dll`, and
`sonypvs1.sys` has now recovered the primary command values:

| Operation | Command byte |
|---|---:|
| play | `0x1a` |
| pause | `0x19` |
| stop | `0x18` |
| rewind | `0x1b` |
| fast-forward | `0x1c` |

The user-mode encoder increments a four-bit sequence and emits
`00 n1 cc n0`; the kernel driver writes that word at the end of the mailbox.
`handycam-core::TransportCommandEncoder` reproduces this algorithm and wraps
the counter modulo 16.

Live validation is complete for all five primary operations. Status byte 2
maps Stop/Play/Pause/Fast-forward/Rewind to `02/06/07/03/83`. Play produced
tape audio and distinct video frames; Pause produced silence and a frozen
frame. Repeated commands and stationary Stops confirmed sequence
acknowledgements through `f9 → 09`, proving hardware wraparound.

The remaining transport work is to identify the extra `0x23`, `0x30`, and
`0x31` commands used by Sony's secondary shuttle/state controls. The one-shot
CLI can recover the current sequence from status byte 0, while a persistent
session API may still be useful for richer applications.

## 3. Quality selection

The DCR-HC24 driver takes the simple path: quality is a one-byte scale factor
at index `0x007c`, configured from 4 through 128. Larger values lower quality.
The Rust core now validates this range and creates the register write.

Fixed startup scale factors 4, 20, 40, 80, and 128 are now live-validated.
Every sample produced 147–148 decodable frames at essentially 25 fps. For the
scene present during the test, average compressed sizes were:

| Scale factor | Approximate bytes/frame |
|---:|---:|
| 4 | 15,971 |
| 20 | 6,288 |
| 40 | 4,423 |
| 80 | 3,462 |
| 128 | 2,971 |

This monotonic result and complete decode success confirm that `0x007c` is the
independent quality control. The production CLI exposes it as a per-session
startup option. Live switching remains unexposed: we still need to determine
whether a mid-stream write briefly stops the encoder or can be applied at a
frame boundary.

Offline comparison is complete. Normal quality initializes `0x007c` to 20
and dynamically descends as low as 9. The low-quality calibration initializes
it to 80 and subsequently ranges from 78 through 90. Both traces upload
identical `0x0080`/`0x00c0` matrices, so the scale factor is the independent
quality control for this camera.

Keep alternate-setting bandwidth separate from image quality. Alternate 5 is
the verified transport allocation; changing it at the same time as the scale
factor would confound two independent controls.

## 4. Audio and A/V synchronization

The audio stream is standard stereo signed PCM16LE at 16 kHz. Endpoint
`0x83` supplies 64 bytes per one-millisecond USB frame:

```text
16 stereo sample frames/ms × 2 channels × 2 bytes = 64 bytes/ms
```

A normal 40-ms video interval therefore spans exactly 640 stereo sample
frames or 2,560 bytes. `handycam-core` now contains the format constants,
an `AudioClock`, and this conversion as tested platform-neutral primitives.

The implemented synchronization authority is:

1. scheduled USB-frame number when the backend supplies it;
2. monotonic capture timestamp plus counted audio samples otherwise;
3. Sony's 11-bit timestamp as a discontinuity/health check, not the sole
   presentation clock.

This ordering matters because the existing OHCI trace contains a seven-second
run of distinct, decodable video records with a frozen Sony timestamp.

The Linux implementation uses a separate audio adapter, so video interface 0
remains under libusb while `snd-usb-audio` owns interfaces 1 and 2. Direct ALSA
capture maps monotonic PCM status timestamps and the counted 16-kHz sample
clock onto the video clock. An `arecord` fallback provides explicitly marked
estimated timestamps.

Raw MJPEG stdout and V4L2 carry video only. The `capture` command writes
timestamped MJPEG and PCM16LE directly to Matroska; applications can also
consume the physical ALSA node alongside the video-only outputs.

Concurrent hardware integration was first verified when FFmpeg recorded 126 live
MJPEG frames plus stereo 16-kHz PCM into a 5.039-second Matroska file. Both
streams began at the same FFmpeg-normalized origin and the audio contained
real signal. That established simultaneous ownership and basic integration,
but did not establish long-run phase-error bounds.

A 20-second follow-up produced exactly 500 video frames and 320,013 extracted
stereo audio sample frames (about 20.0008 seconds). The sub-millisecond
difference was encouraging but included container cutoff rounding. The
same-process correlation described below superseded that early measurement.

`build_sony_av.py` proves the offline clock model. It rebuilt a
9.040-second Matroska file containing 192 distinct 320x240 MJPEG frames and
16-kHz stereo PCM, anchored on the same unwrapped USB frame. Per-frame USB
PTS preserves capture gaps and remains synchronized despite 155 frozen Sony
timestamp intervals. The native libusb transport stamps every endpoint packet
with host monotonic time at callback entry, retains those observations through
frame reconstruction, and correlates them with ALSA capture timestamps in the
platform-neutral synchronizer.

Real-device direct-ALSA capture produced 297 video frames and 297 audio periods
over 11.892 seconds with zero timestamp gaps and a complete FFmpeg decode. At
ten seconds, maximum observed media-clock deviation was 249 us for video and
185 us for audio. Clap tests found a fixed content offset and validated a
60 ms audio delay for this DCR-HC24; see the production
[A/V synchronization document](../../docs/av-sync.md). A 30-minute drift run,
tape-playback calibration, and forced hardware discontinuity tests remain.

## Offline commands

No camera is required for the current analysis:

```sh
python3 ../tools/analyze_sony_driver.py
python3 ../tools/analyze_av_sync.py
python3 ../tools/analyze_sony_control.py /tmp/ohci-control.jsonl
python3 compare_sony_quality.py \
  /tmp/ohci-control.jsonl /tmp/lowq-control.jsonl
python3 annotate_sony_init.py \
  ../../crates/handycam-core/assets/record-mode-init.tsv /tmp/record-mode-init-annotated.tsv
python3 build_sony_av.py \
  ../captures/extracted-record-mode-ohci-all /tmp/sony-av
cargo test -p handycam-core --locked
```

`analyze_av_sync.py` measures endpoint continuity, video timestamp deltas,
USB-frame phase, and repeated-timestamp runs from the existing OHCI
extraction.

The Rust CLI's `replay-capture` subcommand is the no-camera regression
backend. Unlike JPEG replay, it feeds preserved `0x81`/`0x82` descriptors
through the production framing and reconstruction core before reaching
stdout or V4L2.
