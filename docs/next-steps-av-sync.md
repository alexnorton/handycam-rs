# Next steps: production audio/video synchronization

## Current state

The driver currently exposes video as an MJPEG or YUYV byte stream. Audio is
left on the camera's standard ALSA interfaces and is muxed externally by
FFmpeg. This is useful for experimentation, but the two streams do not yet
share a production timestamp model.

The camera protocol provides a video timestamp in each stream header. In
record mode it normally advances by 40 ms per frame (25 fps), and the core
already unwraps the camera's 11-bit timestamp across rollover. At 16 kHz,
640 stereo audio frames correspond to the same 40 ms interval.

The current stdout path does not preserve the camera timestamp. FFmpeg
generates video timestamps from the pipe's nominal frame rate while ALSA
supplies audio timestamps from a separate clock. This can create both a fixed
start offset and gradual drift.

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

### 3. Capture timestamped audio (not started)

The Linux backend should read ALSA period timestamps when available and attach
them to PCM blocks. If hardware timestamps are unavailable, use monotonic read
time plus the known 16 kHz sample count and mark that lower-accuracy mode.

Audio blocks should carry a monotonic start timestamp, sample rate, channel
count, complete sample count, and overrun/discontinuity information.

This needs the `alsa` crate (a safe wrapper over `alsa-lib`), which in turn
needs `libasound2-dev`/`pkgconf` present wherever the CLI is built, so it can
link. Neither is installed in this sandbox; adding the dependency and the
`handycam-alsa` crate is left for a follow-up with that build dependency
available (and, for hardware-timestamp/xrun validation, real camera hardware).

### 4. Add a native muxing path (muxer done; not yet wired to a capture command)

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
synthetic fixture but not yet wired into a CLI `capture` command, since that
also needs Phase 3's ALSA capture to have real audio to mux.

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

Timestamp/event plumbing and deterministic tests are small to medium work.
ALSA timestamped capture is medium work. Native Matroska muxing, reconnect
behavior, and hardware validation are medium to large work.

The protocol reverse engineering is not the main remaining blocker. The work
is primarily timestamp ownership, cross-clock measurement, and packaging the
two existing streams into one correctly timed output.
