# Sony DCR-HC24 userspace driver

This project provides a Rust userspace driver tested with the Sony DCR-HC24
Handycam. It captures the camera's vendor USB video stream and exposes it as:

- clean MJPEG or YUYV frames on stdout;
- a v4l2loopback device on Linux;
- synchronized MJPEG/PCM capture in a Matroska container; and
- tape-playback transport controls for the five primary operations.

The platform-neutral protocol and synchronization core is separate from the
native libusb, ALSA, Matroska, and Linux output adapters. The core builds for
`wasm32-unknown-unknown`; only the Linux platform adapters are implemented.

## Camera compatibility and capture quality

Only the DCR-HC24 has been tested. Sony's recovered Windows XP driver binds to
the USB Streaming interface ID `054c:00c0`, rather than to a retail model name,
and contains three internal hardware profiles. Community hardware reports show
the same ID on cameras including the DCR-HC21, DCR-HC23/HC23E, DCR-HC37,
DCR-HC40E, DCR-HC90, DCR-PC330E, DCR-TRV250, and DCR-TRV461E. This is evidence
that the protocol was shared, but it is not a compatibility guarantee: this
driver currently replays an initialization sequence captured from a DCR-HC24,
and another hardware profile may require different register values.

If an untested camera reports `054c:00c0` in USB Streaming mode, a short
`handycam stream` run is a reasonable compatibility test. Please report the
exact model, USB descriptors, and result. The evidence behind the candidate
list is recorded in the [protocol documentation](reverse-engineering/PROTOCOL.md#compatibility-evidence).

For transferring MiniDV tape, prefer the camera's i.LINK/IEEE 1394 (FireWire)
connection whenever possible. FireWire carries the native DV stream and gives
substantially better archival quality than this camera's 320×240 USB Streaming
video. This driver is useful when FireWire is unavailable and for webcam-style
live capture.

## Build and test

Install Rust, `libusb-1.0` and ALSA development files, V4L2 headers, and (for
the virtual-camera path) v4l2loopback:

```sh
make rust
make rust-check
```

`rust-check` runs formatting, Clippy, the full workspace test suite, and the
WASM core build. The release binary is `target/release/handycam`.

## Live record-mode capture

Install the reviewed udev rule before connecting the camera:

```sh
sudo install -Dm644 contrib/99-handycam.rules \
  /etc/udev/rules.d/99-handycam.rules
sudo udevadm control --reload-rules
```

With the camera in USB record/stream mode, direct MJPEG preview is:

```sh
target/release/handycam stream \
  --output - \
  --semantic-init \
  --quality 20 |
ffplay -f mjpeg -framerate 25 -i -
```

The semantic initializer polls real startup acknowledgements. Omit
`--semantic-init` to use the exact literal fallback plan. `--quality` accepts
Sony scale factors 4 through 128; larger values produce smaller JPEGs.

For a media file:

```sh
target/release/handycam stream --output - --semantic-init --quality 20 |
  ffmpeg -f mjpeg -framerate 25 -i - -c:v copy handycam.mkv
```

The uncompressed compatibility path is:

```sh
target/release/handycam stream --output - --format yuyv --semantic-init |
  ffplay -f rawvideo -pixel_format yuyv422 \
    -video_size 320x240 -framerate 25 -i -
```

## Virtual camera

Load v4l2loopback with a stable device number:

```sh
sudo modprobe v4l2loopback \
  video_nr=10 \
  card_label="Sony DCR-HC24" \
  exclusive_caps=1
```

Then run the producer:

```sh
target/release/handycam stream \
  --output /dev/video10 \
  --semantic-init \
  --quality 20
```

With `exclusive_caps=1`, start the producer first; the node advertises video
capture capability after its first frame. A consumer can then use:

```sh
ffplay -f v4l2 -framerate 25 -video_size 320x240 /dev/video10
```

The driver does not load kernel modules or run privileged commands.

## Tape playback

Switch the camera to tape playback mode and use its mode-specific initializer:

```sh
target/release/handycam stream \
  --output - \
  --playback-init \
  --quality 20 |
ffplay -f mjpeg -framerate 25 -i -
```

Transport commands can be issued from another terminal:

```sh
target/release/handycam transport status
target/release/handycam transport play
target/release/handycam transport pause
target/release/handycam transport stop
target/release/handycam transport fast-forward
target/release/handycam transport rewind
```

The command derives the next four-bit sequence from the status mailbox. Use
`--current-sequence` when reproducing an exact protocol trace. Play, Pause,
Stop, Fast-forward, and Rewind are live-validated. Sony's secondary shuttle
commands and tape metadata are not decoded or exposed.

## Audio and synchronized A/V

The camera exposes standard stereo signed PCM16LE audio at 16 kHz on its
ALSA USB device. The userspace video driver claims only vendor interface 0,
so ALSA can retain the audio interfaces:

```sh
arecord -l
arecord -D hw:1,0 -f S16_LE -r 16000 -c 2 capture.wav
```

The native capture path preserves the observed startup offset and writes
camera-derived video PTS plus sample-clock-derived audio PTS:

```sh
target/release/handycam capture \
  --output handycam-synced.mkv \
  --audio-device hw:1,0 \
  --audio-delay-ms 60 \
  --semantic-init \
  --quality 20
```

The output path must not already exist. Direct ALSA capture is the default and
derives each period's first-sample time from the monotonic PCM status timestamp,
captured-frame availability, and sample count. Use `--audio-backend arecord`
to select the lower-accuracy process fallback when direct ALSA is unavailable.
The direct backend requires ALSA development files when building from source.
`--audio-delay-ms` shifts audio only in the Matroska timeline: positive values
delay audio and negative values advance it. Repeated clap tests with the
DCR-HC24 validated `--audio-delay-ms 60` subjectively and frame by frame. The
default remains `0`, because fixed capture latency may differ by camera model,
mode, USB controller, and backend.

This is a fixed-latency calibration, not clock-drift correction. ALSA can place
audio samples close to their hardware capture time, while a visible frame must
pass through sensor exposure/readout, camera-side JPEG encoding and buffering,
USB packetization, and host-side frame assembly. The Sony decoder also cannot
emit a completed JPEG until the following record header arrives. Those stages
can make the image content appear later than the corresponding sound even when
both clocks advance at exactly the correct rate. Delaying audio by 60 ms aligns
presentation without changing either media clock or the reported drift.

V4L2 and raw MJPEG stdout carry video only. To capture a multiplexed file in
record mode through the compatibility path, use the video pipe and ALSA as two
FFmpeg inputs:

```sh
target/release/handycam stream \
  --output - --semantic-init --quality 20 |
ffmpeg \
  -thread_queue_size 512 -f mjpeg -framerate 25 -i pipe:0 \
  -thread_queue_size 512 -f alsa -ar 16000 -ac 2 -i hw:1,0 \
  -map 0:v:0 -map 1:a:0 -c:v copy -c:a pcm_s16le \
  -af aresample=async=1 handycam-record.mkv
```

For tape playback, replace `--semantic-init` with `--playback-init` and start
transport separately when needed:

```sh
target/release/handycam stream \
  --output - --playback-init --quality 20 |
ffmpeg \
  -thread_queue_size 512 -f mjpeg -framerate 25 -i pipe:0 \
  -thread_queue_size 512 -f alsa -ar 16000 -ac 2 -i hw:1,0 \
  -map 0:v:0 -map 1:a:0 -c:v copy -c:a pcm_s16le \
  -af aresample=async=1 handycam-playback.mkv

target/release/handycam transport play
```

For a live preview while recording, add a second FFmpeg output with the tee
muxer and pipe it to `ffplay`:

```sh
target/release/handycam stream --output - --semantic-init --quality 20 |
ffmpeg \
  -thread_queue_size 512 -f mjpeg -framerate 25 -i pipe:0 \
  -thread_queue_size 512 -f alsa -ar 16000 -ac 2 -i hw:1,0 \
  -map 0:v:0 -map 1:a:0 -c:v copy -c:a pcm_s16le \
  -af aresample=async=1 -f tee \
  "[f=matroska]handycam.mkv|[f=matroska]pipe:1" |
ffplay -fflags nobuffer -f matroska -i -
```

The external FFmpeg path still treats the audio and video clocks independently,
so a small fixed offset may be needed for a particular host; see the production
driver guide for details.

## Documentation

- [Production driver guide](docs/production-driver.md)
- [A/V synchronization design and validation](docs/av-sync.md)
- [Protocol and reverse-engineering record](reverse-engineering/PROTOCOL.md)
- [Reverse-engineering evidence and experiments](reverse-engineering/)

The reverse-engineering directory contains the preserved traces, driver
images, VM notes, analyzers, and historical implementation. It is deliberately
separate from the production Rust path.
