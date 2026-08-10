# Audio/video synchronization

The `handycam capture` command owns the camera video stream and an ALSA audio
source, synchronizes them on a shared monotonic timeline, and writes MJPEG plus
stereo PCM16LE to Matroska. This is the preferred Linux file-capture path. The
video-only `stream` command remains available for stdout, V4L2, and external
FFmpeg workflows.

## Timestamp model

Synchronization lives in `handycam-core`, independently of Linux capture APIs:

- `MonotonicTimestamp` is a session-relative microsecond value;
- `TimedVideoFrame` retains the Sony timestamp and the host observation time;
- `TimedAudioChunk` carries the audio format, sample position, first-sample
  observation time, timestamp accuracy, and discontinuity state; and
- `AvSynchronizer` converts both event streams into container PTS while
  preserving their observed startup offset and reporting clock deviation.

The Linux USB backend timestamps packets at libusb callback entry. The direct
ALSA backend uses monotonic PCM status time, captured-frame availability, and
sample count to locate the first sample in each period. It captures
nonblocking PCM and marks the next chunk discontinuous after XRUN recovery.
`--audio-backend arecord` is an explicit fallback whose timestamps are
estimated from process delivery time.

The core builds for `wasm32-unknown-unknown` and does not refer to ALSA or
libusb. A port must supply video and audio events in one monotonic clock domain:

| Platform | Possible video adapter | Possible audio adapter |
| --- | --- | --- |
| Linux (implemented) | libusb | ALSA |
| macOS | libusb or IOKit | Core Audio |
| Windows | libusb | WASAPI |
| Browser | WebUSB | Web Audio or `getUserMedia()` |

The non-Linux entries describe the adapter boundary, not implemented backends.
Wall-clock time is unsuitable because it can jump during a capture.

## Capture

```sh
target/release/handycam capture \
  --output handycam-synced.mkv \
  --audio-device hw:1,0 \
  --audio-delay-ms 60 \
  --semantic-init \
  --quality 20
```

The output path must not already exist. Direct ALSA is the default. The file
contains camera-derived video PTS and sample-clock-derived audio PTS and is
finalized cleanly on SIGINT.

## Fixed alignment calibration

Repeated DCR-HC24 clap captures placed the audio peak about 40–70 ms before
visible hand contact. `--audio-delay-ms 60` was subjectively and frame-by-frame
validated for this camera. Positive values delay audio in the container;
negative values advance it. The default remains zero because this calibration
may differ by camera, mode, USB controller, and platform backend.

This fixed offset is not evidence of clock drift. Audio timestamps locate PCM
near its hardware capture position, while the corresponding image must pass
through sensor exposure/readout, JPEG encoding and camera buffering, USB packet
assembly, and host reconstruction. The Sony decoder also completes a JPEG only
when the following record header establishes its end. These stages can make
image content appear later than sound even when both clocks advance correctly.
The delay option changes presentation PTS without changing sample counts,
camera timestamps, or drift diagnostics.

Calibrate with a short visible and audible event after changing the camera,
capture mode, USB hardware, or backend. A roughly constant offset calls for
`--audio-delay-ms`; an offset that grows with elapsed time indicates drift.

## Real-device validation

Validation on 2026-08-10 used a DCR-HC24 (`054c:00c0`) and ALSA `hw:1,0`:

- a 6.9-second native capture produced 171 video frames and 172 audio periods,
  zero timestamp gaps, a preserved 38 ms startup difference, and a correctly
  finalized Matroska duration;
- a direct-ALSA 11.892-second capture produced 297 video frames and 297 audio
  periods, zero gaps, and a complete FFmpeg audio/video decode;
- at ten seconds, maximum observed media-clock deviation was 249 us for video
  and 185 us for audio; and
- estimated and direct-ALSA clap captures both showed the fixed 40–70 ms
  content offset described above, with 60 ms subjectively aligned.

The automated suite covers timestamp rollover, dropped ranges, startup offset,
arrival jitter, audio format validation, and Matroska duration finalization.

## Known limits and remaining validation

- The measured hardware runs are short. A 30-minute capture is needed to put a
  practical bound on accumulated A/V drift.
- Record-mode clap alignment is validated; tape-playback alignment and a
  transport transition have not received the same calibration.
- XRUN recovery is implemented and unit-tested, but forced hardware XRUN and
  device-removal behavior need real-device fault tests.
- The Matroska writer produces a streaming file with explicit timestamps and
  duration, but does not write a seek index (`Cues`).
- Only the Linux libusb/ALSA platform adapters are implemented.

Raw MJPEG stdout and V4L2 remain video-only by design. The external FFmpeg
workflow is retained for compatibility, but it timestamps two independent
inputs and therefore does not preserve the same measured startup relationship.
