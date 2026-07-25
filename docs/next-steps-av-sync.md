# Next steps: production audio/video synchronization

## Current state

All five sections below are implemented in code and unit-tested; what
remains is validation against a real DCR-HC24 and its ALSA interface, which
this development sandbox does not have (no camera, no `/proc/asound`).

The driver still exposes video as an MJPEG or YUYV byte stream via `stream`,
unchanged, for FFmpeg compatibility. It now also has a native
`handycam capture --output out.mkv --alsa-device hw:X,Y` path that owns both
the USB video session and an ALSA audio capture thread, and writes a single
timestamped Matroska file with both tracks sharing one clock -- no external
FFmpeg process, no independent per-input clocks.

The camera protocol provides a video timestamp in each stream header. In
record mode it normally advances by 40 ms per frame (25 fps), and the core
already unwraps the camera's 11-bit timestamp across rollover. At 16 kHz,
640 stereo audio frames correspond to the same 40 ms interval.

The old stdout path does not preserve the camera timestamp: FFmpeg generates
video timestamps from the pipe's nominal frame rate while ALSA supplies audio
timestamps from a separate clock, which can create both a fixed start offset
and gradual drift. That path is retained as documented (with a calibration
procedure) for consumers that specifically want the FFmpeg pipeline; `capture`
is the fix for everyone else.

## Recommended implementation

### 1. Define a common timestamp model (done)

- Represent timestamps internally as a monotonic duration or integer
  microseconds.
- Preserve the raw Sony timestamp and its unwrapped value on every frame.
- Record when the first valid video frame and first audio period are observed.
- Make stream start an explicit synchronization event rather than treating
  each input's first packet as time zero.

Implemented in `crates/handycam-core/src/sync.rs` as `Synchronizer`. Each
stream's first observed packet fixes its own offset from a shared `t = 0`;
later packets are placed on the timeline using each stream's own precise
internal clock (the unwrapped camera millisecond timestamp for video, the
sample-frame count for audio), not per-packet host-clock jitter.

### 2. Add timestamp plumbing to the platform-neutral core (done)

- Extend frame/output events with presentation timestamps and durations.
- Keep protocol decoding independent of ALSA, libusb, V4L2, or FFmpeg.
- Add a synchronizer that maps camera timestamps onto the common monotonic
  timeline, handles the 2048 ms camera-timestamp rollover, and reports gaps.
- Test rollover, dropped frames, startup offset, and clock-rate differences.

`Synchronizer::push_video`/`push_audio` wrap the existing `CompressedFrame`
and `AudioChunk` types (`VideoSyncFrame`/`AudioSyncChunk`) rather than
modifying them, so `stream`/`replay`/`replay-capture` stay untouched and
backward-compatible. `handycam-core` has no ALSA/libusb dependency and stays
`forbid(unsafe_code)` and wasm-buildable. Rollover, dropped-frame gaps,
startup-offset alignment, and injected audio-xrun discontinuities are covered
by hardware-free unit tests in `sync.rs`; `AudioClock::resync` lets a real
capture backend report lost samples explicitly instead of faking contiguity.

### 3. Capture timestamped audio (implemented; not yet hardware-validated)

The Linux backend should read ALSA period timestamps when available and attach
them to PCM blocks. If hardware timestamps are unavailable, use monotonic read
time plus the known 16 kHz sample count and mark that lower-accuracy mode.

Audio blocks should carry a monotonic start timestamp, sample rate, channel
count, complete sample count, and overrun/discontinuity information.

Implemented in the new `handycam-alsa` crate as `AlsaCapture`, using the
`alsa` crate (a safe wrapper over `alsa-lib`; `cpal` was considered but
deliberately hides the hardware-timestamp and explicit xrun-recovery APIs
this needs to stay cross-platform). It enables ALSA's `SND_PCM_TSTAMP_TYPE_MONOTONIC`
hardware timestamps when the driver supports them and falls back to a
monotonic read-time estimate otherwise, reporting which one was used via
`TimestampQuality`. An xrun (`EPIPE` on read) is recovered via
`PCM::recover`, and the lost sample-frame span is estimated from elapsed
wall-clock time and applied via the new `AudioClock::resync` from Phase 1,
surfaced to the caller as `AudioCaptureEvent::Overrun`.

This needed the `alsa` crate, which in turn needs `libasound2-dev`/`pkgconf`
present wherever the CLI is built, so it can link; both are now installed in
this environment. There is no ALSA device at all in this sandbox (no
`/proc/asound`), so only the hardware-free parts (`clock` module: timestamp
conversion, lost-frame estimation) have automated tests here. `AlsaCapture`
itself -- hardware-timestamp availability, real xrun recovery, and actual
device I/O -- still needs validation against the real DCR-HC24's ALSA
interface.

### 4. Add a native muxing path (implemented; not yet hardware-validated)

Add an optional `capture` mode that owns both video and ALSA streams and writes
a timestamped container, initially Matroska. Keep `stream --output -` for
simple video consumers and FFmpeg compatibility.

The muxer should write camera-derived video PTS and duration, ALSA-derived
audio PTS and sample counts, preserve an initial offset, report discontinuities
and drift, and stop cleanly on SIGINT, camera removal, or audio loss.

The first implementation can use an existing Rust container crate; the
synchronization model should remain in the platform-neutral core for reuse by
other backends.

Implemented in the new `handycam-mkv` crate as `MatroskaWriter`: a minimal,
dependency-free (aside from `thiserror`), `forbid(unsafe_code)` streaming
EBML/Matroska writer covering exactly one MJPEG video track and one PCM audio
track, using unknown-size `Segment`/`Cluster` elements so it only needs
`Write`, not `Seek` -- no existing Rust crate both writes Matroska and stays
pure-Rust, so this is hand-rolled rather than a dependency. There is
deliberately no `Cues` element yet. It is unit-tested byte-for-byte against a
synthetic fixture and is now wired into a `handycam capture --output out.mkv
--alsa-device hw:X,Y` command (`crates/handycam-cli/src/main.rs`): a
`Synchronizer` and `MatroskaWriter` are driven from the thread polling the USB
video session, draining the ALSA capture thread's channel each tick, sharing
one `Instant` epoch so both `SessionEvent::Packet::received_at` and
`AlsaCapture`'s `captured_at` land on the same `HostNanos` axis. `stream
--output -` is untouched. End-to-end verified in this sandbox by running the
real binary (no camera or ALSA device present): it opens the output file,
writes a byte-correct EBML header/Tracks section (checked against the
hand-decoded bytes), and exits cleanly with an error instead of hanging when
the camera can't be opened. What remains is exactly the real-hardware
measurement plan below.

### 5. Retain an FFmpeg compatibility mode

Until native muxing is complete, keep the documented external FFmpeg workflow
and add an explicit audio-offset option for calibration. For example,
`-itsoffset 0.25` applied to the ALSA input delays audio by 250 ms. Do not
make a guessed offset the default. `aresample=async=1` can correct small clock
differences, but cannot discover the initial offset.

## Measurement and test plan

1. Record a clap, bell, or LED/flash visible in both streams.
2. Measure the first-event offset with `ffprobe` or waveform inspection.
3. Repeat at 30 seconds, 5 minutes, and 30 minutes to separate fixed offset
   from clock drift.
4. Repeat in record and tape-playback modes, including a transport transition.
5. Test audio overrun, dropped video frames, timestamp rollover, and reconnect.
6. Compare native output against the external FFmpeg reference pipeline.

## Acceptance criteria

- Initial A/V alignment is within 40 ms without a user-supplied offset.
- At 30 minutes, drift is below one video frame or is corrected transparently.
- A dropped packet or audio overrun produces a detectable discontinuity.
- The stdout video path remains backward-compatible.
- Core synchronization tests run without hardware or kernel modules.

## Rough effort

Timestamp/event plumbing, ALSA capture, native Matroska muxing, and the
`capture` CLI wiring are all implemented and code-reviewed against
hardware-free tests. What is left is entirely the real-hardware measurement
and validation plan above: confirming hardware timestamps and xrun recovery
against the camera's actual ALSA interface, and running the clap/flash
alignment and drift measurements this document specifies. That is medium
work, but it is measurement and possible bugfixing against real hardware, not
new design or implementation.
