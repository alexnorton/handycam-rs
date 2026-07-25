# Integration, live audio, and transport recovery

Date: 2026-07-25  
Camera: Sony DCR-HC24, `054c:00c0`  
Host video sink: `/dev/video10`, v4l2loopback 0.15.3

This pass joined the already-tested pieces into complete consumer-facing
pipelines, established the live Linux audio format, tested the strongest
camera-free protocol replay, and recovered playback commands from Sony's
preserved binaries.

## V4L2 integration

Recorded JPEG replay through the production MJPEG V4L2 sink was consumed by
FFmpeg for 100 frames:

```text
codec:       MJPEG
dimensions:  320x240
rate:        25 fps
duration:    4.000 s
SHA-256:     7c6b96237ac22d55022495889d7c6d078559a5f857cde623f7e6a56620ddd1e1
```

The YUYV compatibility path was replayed through the same node and captured
as 50 FFV1 frames:

```text
pixel format: yuv422p
dimensions:   320x240
rate:         25 fps
duration:     2.000 s
SHA-256:      21369f2911f65f00737af9837cd0a9d7bc004488c9692e14ca3c5c277a141789
```

The complete raw endpoint fixture was then paced through:

```text
iso-packets.jsonl + ep81.bin + ep82.bin
  -> StreamDecoder
  -> JPEG reconstruction
  -> V4L2 MJPEG
  -> FFmpeg consumer
```

The producer decoded all 1,455 preserved packets into 123 frames with zero
protocol errors. FFmpeg captured 90 of them:

```text
codec:       MJPEG
dimensions:  320x240
rate:        25 fps
duration:    3.603 s
SHA-256:     f462eacd1e8a301176538f65b1bc211634135a8f22c73744059d265b9d7bed4c
```

This is the strongest hardware-free integration test: it retains the original
cross-endpoint packet boundaries and exercises every production layer after
USB submission.

## Live standard USB audio

Linux bound camera interfaces 1 and 2 to `snd-usb-audio` while vendor
interface 0 remained available for libusb. Direct ALSA capture succeeded:

```sh
arecord -D hw:1,0 -f S16_LE -r 16000 -c 2 -d 5 \
  /tmp/handycam-audio-baseline.wav
```

Result:

```text
format:      signed PCM16LE
channels:    2
rate:        16,000 Hz
duration:    5.000 s
PCM bytes:   320,000
SHA-256:     639d9c0f797d702fb9823bd48682d4793dc9d9450d6db2bbf28322809a9c2222
```

FFmpeg also consumed replayed V4L2 video and live camera ALSA audio together
for five seconds:

```text
video:       320x240 MJPEG at 25 fps
audio:       stereo PCM16LE at 16 kHz
duration:    5.000 s
SHA-256:     9502c627f32d4ddc1e18e5656e1d316d044870cb99076a14ac4f0732fecda237
```

This proves the non-exclusive Linux ownership model. It does not yet prove
long-run live video/audio phase because the video source in this test was the
fixture replay.

## Playback command recovery

The relevant preserved binaries are:

```text
CapturingTool.exe  0d1835d2a1337392c88473a5ca4ace48712267499a0e1348214dd2e973162e62
USB01.dll          6ae4695df97943f7f7b56fee4a7413e7d0f1b01c5bde6f687e8cc598af1aa09a
S2UCapture.dll     d79620209b3d8a628ee587a9cb9911ee94373ca3cbc456181c626a56dbd54c48
sonypvs1.sys       81bcba6de9cf540c66b4226bd5d46084295f41822bc1a7eb938277737f46cc76
```

`analyze_pe_resources.py` decodes the main dialog and identifies the named
Play, Fast-forward, Rewind, Stop, and Pause controls. Their handlers in
`CapturingTool.exe` pass `0x1a`, `0x1c`, `0x1b`, `0x18`, and `0x19`
respectively through the input plug-in.

`S2UCapture!_S2UCommand` maps its application enum to the same values. Its
internal command method increments a four-bit counter, duplicates it around
the operation byte, and sends property `0x15`:

```text
00 n1 cc n0
```

Finally, `sonypvs1!CustomPropCommandWrite` stores that 32-bit word in
big-endian order at bytes 60–63 of a zeroed 64-byte block and writes request
`0x88` at index `0x0300`.

Recovered primary operations:

| Operation | `cc` |
|---|---:|
| Play | `0x1a` |
| Pause | `0x19` |
| Stop | `0x18` |
| Fast-forward | `0x1c` |
| Rewind | `0x1b` |

This also identifies the record-mode paired family `00 n1 18 n0` as
sequenced Stop commands. `handycam-core` now provides a typed
`TransportCommand` and wrapping `TransportCommandEncoder`, covered by unit
tests. The additional `0x23`, `0x30`, and `0x31` secondary commands still
need playback-mode behavior and status mapping.

## Native live test interruption

The camera initially enumerated at physical path `001-2`. Its standard audio
path worked, but interface-0 access was denied because the current device
node was `root:root 0664` without a user ACL. The repository udev rule now
includes `TAG+="uaccess"` for an active local session.

After live-detaching the camera from XP, QEMU released it successfully but
the Handycam disappeared from the host USB bus instead of re-enumerating.
The host exposes no safe per-port power control; cycling the xHCI controller
would also disrupt unrelated webcam and Bluetooth devices. A physical USB
reconnect or camera power cycle is therefore required before the remaining
literal-versus-condition init and fixed-quality matrix can run.

The persistent libvirt definition was not changed by the live detach.

## Final camera-free regression

After the live-test interruption, the release build was rebuilt and the raw
endpoint fixture was passed through both production output modes again.

Stdout was piped directly to FFmpeg as an MJPEG elementary stream. FFprobe
reported 123 frames, 320x240, 25 fps, and 4.920 seconds:

```text
SHA-256 (Matroska): ad22f3943b0053189e458f1dbc6b011bd4517fe48a5aa8ed1c2faf128aeccf8e
producer:            1,455 packets, 123 frames, 0 protocol errors
```

Diagnostics remained on stderr, so no logging bytes contaminated stdout.
Scale factors 3 and 129 were also rejected before USB access, confirming both
ends of the supported `4..=128` range.

The host's real `/dev/video10` node was then tested from outside the
restricted build environment:

```text
device:              Sony DCR-HC24 (v4l2loopback)
format:              MJPEG, 320x240, 25 fps
consumer result:     100 frames, 4.000 seconds
SHA-256 (Matroska):  95aab1fe61e9c8e7198e2270150a9268bb2eedbafcde24a8c45575f9bcb99091
producer:            1,455 packets, 123 frames, 0 protocol errors
```

The final quality gate passed formatting, warning-free Clippy, 23 Rust unit
tests, the `wasm32-unknown-unknown` core build, Python compilation, and both
offline protocol analyzers.

For synchronized A/V follow-up, every native libusb packet event now records
host monotonic time at callback entry. That gives a future ALSA adapter a
clock from the same process and avoids treating the Sony 11-bit timestamp as
the presentation clock.

## Reconnected live Linux acceptance

After a physical camera power cycle, the device re-enumerated as
`001-2`/address 9 and remained attached to the Linux host rather than XP:

```text
interface 0: unclaimed vendor video
interfaces 1/2: snd-usb-audio
audio device: hw:1,0
```

The first video attempt failed with `Access denied` because the new usbfs
node was `root:root 0664`. Installing `contrib/99-handycam.rules` and adding a
temporary ACL for the current node fixed the original `ffplay` failure.

### Literal and semantic initialization

The exact literal plan initialized in approximately 2.66 seconds and emitted
107 fully decodable frames during the remainder of a bounded seven-second
run.

Condition polling acknowledged the four startup commands as follows:

| Token | Poll attempts |
|---:|---:|
| `0x71` | 8 |
| `0x81` | 4 |
| `0x91` | 6 |
| `0xa1` | 5 |

It initialized in approximately 1.07 seconds and emitted 147 fully decodable
frames in the same bounded run. This validates the first replacement of
opaque captured reads with a real protocol completion condition.

### Live quality matrix

Each startup scale factor produced 147–148 decodable 320x240 frames at
essentially 25 fps:

| Scale factor | Total bytes | Frames | Approx. bytes/frame |
|---:|---:|---:|---:|
| 4 | 2,347,752 | 147 | 15,971 |
| 20 | 924,270 | 147 | 6,288 |
| 40 | 654,654 | 148 | 4,423 |
| 80 | 508,978 | 147 | 3,462 |
| 128 | 439,674 | 148 | 2,971 |

The monotonic reduction confirms that the one-byte `0x007c` scale factor is
the independent quality control. One 41-ms timestamp step and two expected
first-record discontinuities were observed; all files decoded without JPEG
errors.

### Live output integrations

Direct MJPEG stdout produced 107 literal-init frames and 147 semantic-init
frames with no decoder errors. A visible stdout-to-FFplay run continued for
ten seconds:

```text
USB packets:       20,006
record headers:    250
USB packet errors: 0
timestamp gaps:    0
protocol errors:   0
```

FFplay backpressure dropped 12 frames at the one-slot output queue without
affecting USB capture.

With v4l2loopback `exclusive_caps=1`, the producer must open and supply its
first frame before a consumer opens the node. In that order, live camera to
`/dev/video10` to FFmpeg produced 100 MJPEG frames over exactly 4.000
seconds:

```text
SHA-256: 9a6c711008b075a50572e0262021830437a08b1e4c36e595f766d836950ea859
```

The live YUYV stdout conversion also produced 100 frames over 4.000 seconds,
captured losslessly as FFV1:

```text
SHA-256: 7769374b80fefce3c950b812d8f95beadee412189622837b5e9b51a9903b8968
```

### Concurrent live audio and video

FFmpeg consumed physical `hw:1,0` audio while the userspace driver owned only
video interface 0. The resulting Matroska file contained:

```text
video:      126 MJPEG frames, 320x240 at 25 fps
audio:      stereo PCM16LE at 16 kHz, non-silent
duration:   5.039 seconds
start time: 0 for both streams after FFmpeg normalization
SHA-256:    a3b99338274ce6af337f3412ac33e5351353f86339fd20eedfdbd2c4e88d7159
```

This proves concurrent live ownership and basic A/V integration. Measuring
long-run phase error still requires propagating the new monotonic video
packet times and reading ALSA capture timestamps in the same process.

A subsequent 20-second sample contained exactly 500 video frames. Extracted
audio contained 1,280,052 bytes, or 320,013 stereo sample frames: about
20.0008 seconds at 16 kHz. This sub-millisecond endpoint difference includes
FFmpeg's final packet/cutoff rounding, so it is a useful coarse bound rather
than a direct clock-drift measurement.

During that run the Sony timestamp reported one `0 ms` delta followed by
`81 ms`, while USB delivery and output cadence remained continuous. The pair
effectively caught up, providing live confirmation of the decision not to use
that field as the sole presentation clock.

```text
video:      500 frames, final packet 19.960 s + 0.040 s
audio:      final packet PTS 19.999 s
container:  20.000 s
SHA-256:    7ffb7511aed418c2c796932888ba23874ab5242c4719737e248a765cf2e44c64
```

### Semantic-init soak

A 32-second bounded run reported at its 30-second checkpoint:

```text
USB packets:       60,008
nonempty packets:  6,764
USB bytes:         4,347,912
boundaries:        752
record headers:    751
completed frames:  749
packet errors:     0
timestamp gaps:    0
queue drops:       0
protocol errors:   0
```

Shutdown restored interface 0 without requiring a reconnect.

## Live playback-mode transport and capture

The camera was switched to tape playback without re-enumerating. USB path,
ACL, and standard audio bindings remained unchanged.

### Status-only baseline

A read-only request to `0x0340` succeeded before any Linux command:

```text
a9 01 02 01 99 00 04 01 99 00 00 ... 00
```

Only bytes 0–9 were active, consistent with the preserved Windows trace.

### Primary transport operations

The experimental native control path reads status, writes exactly one
64-byte command mailbox, waits, then reads status again. Every recovered
operation changed the physical transport and status byte 2:

| Operation | Command word in this run | Status byte 2 |
|---|---:|---:|
| Stop | `00111810` | `02` |
| Play | `00211a20` | `06` |
| Pause | `00311930` | `07` |
| Fast-forward | `00511c50` | `03` |
| Rewind | `00711b70` | `83` |

Play produced non-silent tape audio at approximately -40 dB RMS. Pause
changed the same endpoint to exact digital silence. Fast-forward and Rewind
were each followed by Stop.

Status byte 0 correlates commands with the sequence nibble. Repeated
stationary Stop commands produced:

```text
b9 -> c9 -> d9 -> e9 -> f9 -> 09
```

The final word was `00011800`, proving the four-bit encoder wraps on the real
camera exactly as recovered from `S2UCapture`.

### Mode-specific startup acknowledgement

Record-mode semantic polling waits for `n1`. In playback, the same first
command remained at status `79` rather than reaching `71`, causing the
record-mode condition to time out safely. Literal replay still initialized
and produced 108 decodable stopped-tape frames.

A separate playback strategy was then implemented. It waits for the `n9`
echo:

| Command token | Playback acknowledgement | Attempts |
|---:|---:|---:|
| `71` | `79` | 6 |
| `81` | `89` | 5 |
| `91` | `99` | 5 |
| `a1` | `a9` | 5 |

Playback semantic initialization completed in approximately 1.04 seconds and
produced 149 decodable frames in a bounded seven-second stopped-tape run.

### Concurrent control and playback capture

Literal initialization was used for a single capture while Play, Pause, and
Stop were issued from a second control handle. The output contained 258
decodable MJPEG frames and 88 unique decoded-pixel hashes:

```text
stopped prefix: one repeated image
Play:           spin-up, then 87 distinct motion frames
Pause:          final tape frame held
Stop:           original stopped image restored
SHA-256:        2abff783cfd3bdcc37a07c7a89bccb54c668dfd34a42f5c69925054d1dd848a2
```

The transport transitions were independently present in status byte 2:
`02 → 06 → 07 → 02`.

Playback video timestamps naturally include 39-ms steps. The CLI health band
now treats 39–41 ms as nominal while retaining warnings for real transition
discontinuities such as the observed 33/66-ms Play/Pause pair.

A final release-build repetition used playback-specific polling and the
500-ms default transport observation delay. It captured 274 frames while
status byte 2 reported the settled transitions `02 → 06 → 07 → 02`.
Only three transition timestamp gaps were logged.

FFmpeg emitted one recoverable entropy-concealment warning in each of the two
concurrent transport runs. Pillow decoded every individual JPEG in both
files, and the native stream reported zero packet or protocol errors. This is
therefore recorded as a tape-transition JPEG anomaly rather than USB framing
loss; a production container path may optionally validate or flag such
frames.

### Production V4L2 plus ALSA playback

A final consumer-facing run used playback-specific initialization to feed
`/dev/video10` while FFmpeg recorded the physical ALSA device. Play, Pause,
and Stop were issued during the recording:

```text
video:           201 MJPEG frames, 69 unique decoded-pixel hashes
audio:           stereo PCM16LE at 16 kHz, non-silent during Play
duration:        8.021 seconds
transport state: 02 -> 06 -> 07 -> 02
packet errors:   0
protocol errors: 0
decode errors:   0
SHA-256:         c8670b8b92f4c96ce1f35ca2a072763628dd2b42624b54bf473fb1d5ec0ba943
```

The one-shot CLI was then changed to derive the current sequence from the
high nibble of status byte 0 when no override is supplied. Three independent
commands proved the stateless workflow:

```text
status 39 -> Play  word 00411a40 -> status 49 / state 06
status 41 -> Pause word 00511950 -> status 51 / state 07
status 51 -> Stop  word 00611860 -> status 69 / state 02
```

The tape was left stopped.
