# Next steps: browser-based live monitoring and recording (WebUSB + WebCodecs)

## Current state

This is entirely greenfield: no `wasm-bindgen`, JS/TS, or web/ directory
exists anywhere in this repo, and nothing here has been prototyped. Only two
mentions of this direction exist today, both forward-looking, not
implementation:

- `README.md:10-12`: "The core builds for `wasm32-unknown-unknown`, leaving
  room for WebUSB, macOS, or Windows backends later."
- `docs/production-driver.md:396-397`: "`handycam-core` builds for
  `wasm32-unknown-unknown`. A future WebUSB backend can provide individual
  endpoint packets to the same `StreamDecoder`."

`rust-toolchain.toml` already installs the `wasm32-unknown-unknown` target
workspace-wide, and `make rust-check` already builds `handycam-core` for it,
but no crate depends on `wasm-bindgen` and nothing produces a wasm artifact
today. No CI exists in this repo at all (no `.github/workflows`), for any
target.

Two crates are already `wasm32-unknown-unknown`-clean, with zero `unsafe`
code and no platform-specific dependencies:

- `handycam-core` (`forbid(unsafe_code)`): `StreamDecoder`, `Synchronizer`,
  `AudioClock`, `CompressedFrame`, `AudioChunk`, `HostNanos`,
  `Discontinuity` -- see `docs/next-steps-av-sync.md` for how these were
  built and tested for the native path.
- `handycam-mkv` (`forbid(unsafe_code)`, depends only on `thiserror`):
  `MatroskaWriter`, which needs only `std::io::Write`, never `Seek`.

`handycam-libusb` (native libusb transport) and `handycam-alsa` (ALSA
capture) stay native-only; a browser frontend replaces both with browser
APIs, not with a port of either crate.

The problem this document scopes: reuse the two portable crates above,
unchanged, behind a new `wasm-bindgen` boundary, to support a live-monitoring
preview and a recording-to-file path in a browser tab, with the same
correlated-timestamp discipline the native `capture` command already
established. Two risks matter more than anything else here and must not be
glossed over: WebUSB's isochronous-transfer reliability for this camera is
unproven, and the camera's MJPEG frame format is not a browser-standard
streaming codec, so live monitoring is not simply "wire WebCodecs into MSE"
-- it needs a real transcode step. Both are called out explicitly below.

## Architecture overview

**Reused unchanged**, via a new `handycam-web` wasm-bindgen crate that
depends only on `handycam-core` and `handycam-mkv` (no `handycam-libusb`, no
`handycam-alsa`):

- `handycam_core::StreamDecoder::push_packet` -- fed `EndpointPacket`s
  assembled from WebUSB isochronous transfer results, exactly mirroring how
  `handycam-libusb` feeds it today from native isochronous transfers.
- `handycam_core::Synchronizer::push_video`/`push_audio` -- fed
  browser-derived `HostNanos` (see the clock-correlation step below); no
  change to its "first packet of each stream defines its own offset from a
  shared origin" design.
- `handycam_core::AudioClock::push_pcm`/`resync` -- fed PCM16LE frames
  assembled from an `AudioWorkletProcessor`.
- `handycam_mkv::MatroskaWriter` -- used verbatim for the recording path,
  producing the same V_MJPEG/PCM `.mkv` structure the native `capture`
  command already writes.

**Net-new, browser-only, with no Rust-side precedent today:**

- The WebUSB device/interface claim and isochronous transfer polling loop.
- `getUserMedia` + `AudioWorkletProcessor` audio capture.
- The clock-correlation step tying WebUSB's `performance.now()` domain to
  Web Audio's `AudioContext.currentTime` domain.
- The live-monitoring transcode pipeline: MJPEG decode, WebCodecs
  re-encode, JS muxing, MSE wiring.
- Discontinuity-to-UI surfacing.

No changes to `handycam-core` or `handycam-mkv` source are required for any
of this.

## Recommended implementation

### 1. Spike: validate WebUSB isochronous transfer feasibility (do this first)

This is the single biggest open risk and gates everything after it. The
camera's video data arrives over USB isochronous transfers (not bulk or
interrupt). WebUSB's spec does define `isochronousTransferIn`, but real-world
browser support and reliability for isochronous transfers -- as opposed to
control/bulk/interrupt -- has historically been far less mature, especially
for anything resembling continuous video streaming. Do not assume this
works; validate it before investing in anything else.

Build a throwaway plain-JS prototype (no Rust, no wasm-bindgen) in Chrome:

- `navigator.usb.requestDevice` filtered to `vendorId: 0x054c, productId:
  0x00c0` (`handycam_core::VENDOR_ID`/`PRODUCT_ID`).
- `device.claimInterface(0)`, `device.selectAlternateInterface(0, 5)`,
  mirroring `VIDEO_INTERFACE`/`VIDEO_ALT` and the exact claim/alt-setting
  sequence already implemented natively in
  `crates/handycam-libusb/src/transport.rs`.
- Sustained `isochronousTransferIn` polling on endpoints `0x81` (boundary)
  and `0x82` (video), matching the native ~768-byte packet sizing.
- Measure: sustained multi-minute capture without transfer stalls or errors,
  packet loss rate, and per-poll latency.

WebUSB is Chromium-only (unsupported in Firefox and Safari) -- treat that as
an accepted platform constraint, not something to solve. If isochronous
transfers prove unreliable in practice, this whole plan needs either a
degraded, drop-tolerant mode or to not proceed with WebUSB at all; decide
that explicitly once spike data exists rather than assuming success.

### 2. Scaffold the `handycam-web` wasm-bindgen crate

New workspace member, `wasm32-unknown-unknown` target, depending only on
`handycam-core` and `handycam-mkv`. Thin `#[wasm_bindgen]` wrapper types over
`StreamDecoder`, `Synchronizer`, `AudioClock`, and `MatroskaWriter`, taking
plain byte slices/typed arrays and numeric types across the boundary.
Decide whether `Discontinuity`/`TimestampQuality` are mirrored as
JS-friendly tagged objects or left for the JS side to pattern-match via
simple field checks. Build with `wasm-pack`/`wasm-bindgen-cli`, `--target
web`, since this feeds a plain browser page rather than a bundler-driven SPA
(open question if a specific frontend framework is already intended).

### 3. WebUSB video acquisition loop

Feed each isochronous packet as an `EndpointPacket { endpoint, data }` into
`StreamDecoder::push_packet` across the wasm boundary. Record
`performance.now()` at each transfer completion -- the browser analog of
native `SessionEvent::Packet::received_at: Instant`. Decide a policy for
`StreamDecodeError` surfaced back from wasm (drop frame, log, surface to UI),
mirroring how the native CLI logs and resets the decoder on a malformed
record today.

### 4. Audio acquisition via getUserMedia + AudioWorklet

Audio can never go through WebUSB -- browsers block claiming Audio Class
interfaces over it for security reasons. This is a hard platform constraint,
not an implementation choice, and mirrors the native split where audio comes
from a separate standard USB Audio Class interface handled by ALSA, not the
vendor interface `handycam-libusb` claims.

Use `getUserMedia({ audio: { ... } })` to select the camera's audio input,
and an `AudioWorkletProcessor` to receive 128-frame render quanta. Accumulate
and convert to interleaved PCM16LE, feeding `AudioClock::push_pcm`.
`AudioContext.sampleRate` is commonly 44.1kHz or 48kHz, not the camera's
native 16kHz, so a resampling step is very likely required here -- this has
no native-path analog, since ALSA can simply be asked for 16kHz directly.
Record each render quantum's `AudioContext.currentTime` for the correlation
step below.

### 5. Clock correlation

WebUSB transfer-completion timestamps (`performance.now()`) and Web Audio
timestamps (`AudioContext.currentTime`) are two independent clock domains
with no browser-guaranteed fixed relationship between them -- the browser
analog of the native `capture_epoch: Instant` shared origin in
`crates/handycam-cli/src/main.rs:1000-1260` (`run_capture`,
`host_nanos_since`).

Recommended approach: pick `AudioContext.currentTime` as the reference axis
(Web Audio scheduling and, later, MSE playback timing both ultimately depend
on it). At stream start, sample one `performance.now()` reading and one
`AudioContext.currentTime` reading as close together as achievable (e.g. in
the same microtask/rAF tick) to establish a fixed offset, then convert every
subsequent WebUSB `performance.now()` timestamp into that axis before
converting to nanoseconds and calling `Synchronizer::push_video`/
`push_audio`. This is the direct analog of `Synchronizer`'s existing "first
packet of each stream defines its own offset from a shared origin" design,
already implemented and unit-tested in `crates/handycam-core/src/sync.rs` --
no core logic changes are needed here, only where the `HostNanos` values
come from.

Three unit systems are in play and worth stating explicitly: `Synchronizer`
and `HostNanos` work in integer nanoseconds internally (unchanged);
`AudioContext.currentTime` is fractional seconds; WebCodecs'
`VideoFrame`/`AudioData`/`EncodedVideoChunk`/`EncodedAudioChunk` `timestamp`
fields (used in the monitoring path below) are integer microseconds. All
conversions should happen only at the JS/wasm boundary, never inside
`handycam-core`.

### 6. Recording path -- reuses `handycam-mkv` verbatim

Call `MatroskaWriter::create` with the same `VideoTrackConfig { width: 320,
height: 240 }` and `AudioTrackConfig { sample_rate: 16000, channels: 2,
bit_depth: 16 }` the native `capture` command uses (`handycam_core::WIDTH`/
`HEIGHT`/`AUDIO_SAMPLE_RATE_HZ`/`AUDIO_CHANNELS`), and drive
`write_video_frame`/`write_audio_chunk` directly from `VideoSyncFrame`/
`AudioSyncChunk`. No change to `handycam-mkv` at all.

Since `MatroskaWriter<W: Write>` only needs `Write`, not `Seek`, wire it to
either a growing in-memory buffer or, preferably, the File System Access
API's `FileSystemWritableFileStream` for incremental writes without
buffering an entire recording in memory (also Chromium-primary, consistent
with the WebUSB constraint).

This output file is structurally identical to today's native `.mkv`
(V_MJPEG + PCM/INT/LIT codecs) -- it is not meant to play back live in the
browser's own `<video>` tag, but opens correctly in VLC/ffplay/mpv exactly
like the existing native output. This is the easy path: zero transcoding,
zero new codec work.

### 7. Live-monitoring path -- a real transcode pipeline, not just "MSE it"

State the core problem plainly: MJPEG (what `CompressedFrame.jpeg` and
`handycam-mkv`'s "V_MJPEG" CodecID both are) is not decodable or playable
through `<video>` + MSE, and is not a codec WebCodecs' `VideoDecoder`
registry recognizes -- MSE/WebM is spec-restricted to VP8/VP9/AV1 video and
Opus/Vorbis audio. A naive "pipe WebCodecs into MSE" description does not
work as-is for this project's frame format; this is real, new work, not a
thin wrapper around the recording path.

The required pipeline:

1. Decode each `CompressedFrame.jpeg` using `ImageDecoder`/
   `createImageBitmap` -- image-decoding APIs, deliberately not
   `VideoDecoder`, since MJPEG isn't a registered WebCodecs video codec.
2. Re-encode the decoded image via WebCodecs `VideoEncoder` into VP8
   (patent-free, always available in Chromium, a good match for MSE/WebM).
3. Re-encode the raw PCM (from `AudioSyncChunk`) via WebCodecs
   `AudioEncoder` into Opus.
4. Stamp each resulting `EncodedVideoChunk`/`EncodedAudioChunk`'s
   `timestamp` field from the same `pts_ns` `Synchronizer` already computed,
   converted from nanoseconds to microseconds at this boundary.
5. Mux the encoded chunk streams into a live fragmented WebM via a small JS
   muxer -- evaluate an existing library purpose-built for pairing with
   WebCodecs output versus a minimal hand-rolled one scoped to exactly
   VP8+Opus, mirroring `handycam-mkv`'s own precedent of hand-rolling when no
   existing crate fit.
6. Feed the resulting fragments into an MSE `SourceBuffer` (`video/webm;
   codecs="vp8,opus"`) attached to a `<video>` element.

This is genuine, continuous per-frame CPU cost (JPEG decode + VP8 encode +
PCM-to-Opus encode, at 25fps/16kHz) with no equivalent anywhere in the
native pipeline -- `stream`/`capture` never re-encode anything today. Run
this pipeline in a Web Worker to keep the main thread responsive (WebCodecs
is available in workers). MSE buffering adds further latency on top of
encode latency; this needs to be measured against an explicit live-
monitoring latency target, not just checked for correctness.

### 8. Discontinuity surfacing

`Discontinuity::VideoGap { expected_delta_ms, actual_delta_ms }` and
`Discontinuity::AudioGap { expected_sample_frame, actual_sample_frame }`
already flow out of `Synchronizer::push_video`/`push_audio` unchanged. New
work here is a JS-facing representation and a minimal, rate-limited UI
treatment (e.g. a transient banner) so continuous minor gaps don't spam the
UI -- this is the only user-visible signal that sync has degraded, so it
must exist, but keep its scope deliberately small.

## Measurement and test plan

1. Report the WebUSB isochronous spike's results (step 1) here as an
   explicit early gate: measured packet loss rate, sustained-duration
   limits, any Chrome-version-specific quirks -- not folded into general
   testing.
2. Reuse the native clap/bell/flash test from `docs/next-steps-av-sync.md`
   for the recording path: capture the browser-produced `.mkv` and analyze
   it with `ffprobe`/waveform inspection identically to the native
   baseline, to confirm `MatroskaWriter` reuse produces equivalent alignment
   when fed browser-derived `HostNanos`.
3. For the monitoring path, add a live glass-to-glass latency measurement --
   time from the physical event to on-screen/on-speaker presentation via
   the MSE `<video>` element -- since this path has an entirely new
   pipeline stage with no native equivalent.
4. Run a 30-minute drift test (matching the native acceptance criterion) on
   both paths, specifically to catch clock-correlation drift in the
   `performance.now()`/`AudioContext.currentTime` mapping from step 5 -- a
   new failure mode with no native analog.
5. If `AudioContext.sampleRate` isn't 16kHz (the common case), explicitly
   verify the resampling step doesn't accumulate drift against
   `AudioClock`'s sample-frame count.
6. Reconnect/discontinuity injection tests analogous to native (unplug the
   camera mid-stream, force an audio underrun), checked against both
   `Discontinuity` variants surfacing correctly to the UI layer.
7. No cross-browser test matrix -- WebUSB is Chromium-only, so this isn't
   applicable or possible.

## Acceptance criteria

- The WebUSB isochronous spike (step 1) passing is a hard prerequisite gate
  before any other part of this plan is considered viable; until spike data
  exists, this is the single blocking unknown.
- The recording path's browser-produced `.mkv` matches the native
  `capture` command's existing acceptance criteria: alignment within 40ms
  without a user-supplied offset, drift below one video frame at 30
  minutes.
- The monitoring path's live latency target is left as TBD until measured,
  rather than invented up front; a detectable discontinuity indicator must
  appear in the UI within one render cycle of a real gap.
- Zero required changes to `handycam-core` or `handycam-mkv` source.
- WebUSB/File System Access being Chromium-only is an accepted, documented
  constraint, not a gap to close.
- Existing native hardware-free unit tests continue passing unmodified.

## Rough effort and sequencing

1. WebUSB isochronous spike (plain JS, no Rust) -- small effort, but a hard
   go/no-go gate; do this first.
2. `handycam-web` crate scaffold and build tooling -- can start in parallel
   once the spike looks directionally viable, since it depends on the spike
   not failing outright, not on its final numbers.
3. Recording path (steps 2 partial, 3, 4, 5, 6) -- medium effort, low risk:
   almost entirely wiring already-implemented, already-tested Rust code to
   new transport-layer glue, with no new codec work.
4. Clock correlation (step 5) -- small effort but easy to get subtly wrong;
   needs its own focused validation (test-plan item 4) before either path's
   timestamps can be trusted.
5. Live-monitoring transcode pipeline (step 7) -- the largest and most
   novel effort: JPEG decode, VP8/Opus encode, a JS muxer, MSE wiring, a
   worker-thread architecture, and latency tuning. Scope this only after
   the recording path and clock correlation are validated, since it depends
   on both.
6. Discontinuity UI (step 8) -- small, can trail behind; optional for an
   initial milestone.
7. CI for the new wasm target -- no CI exists in this repo at all today for
   any target; call this out as an explicit follow-up rather than a silent
   gap.

The two named risks -- WebUSB isochronous reliability and the MJPEG/MSE
codec mismatch -- are the dominant cost and uncertainty drivers of this
whole plan. Everything else here is comparatively mechanical wiring of
already-implemented, already-tested Rust code.
