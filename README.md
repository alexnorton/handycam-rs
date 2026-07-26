# Sony DCR-HC24 userspace driver

This project provides a Rust userspace driver for the Sony DCR-HC24 Handycam.
It captures the camera's vendor USB video stream and exposes it as:

- clean MJPEG or YUYV frames on stdout;
- a v4l2loopback device on Linux; and
- experimental tape-playback transport controls.

The platform-neutral protocol core is separate from the native libusb backend
and Linux sinks. The core builds for `wasm32-unknown-unknown`, leaving room
for WebUSB, macOS, or Windows backends later.

## Build and test

Install Rust, `libusb-1.0` development files, V4L2 headers, and (for the
virtual-camera path) v4l2loopback:

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
`--current-sequence` when reproducing an exact protocol trace. The transport
interface remains experimental while secondary shuttle commands and metadata
are investigated.

## Audio and synchronized A/V

The camera exposes standard stereo signed PCM16LE audio at 16 kHz on its
ALSA USB device. The userspace video driver claims only vendor interface 0,
so ALSA can retain the audio interfaces:

```sh
arecord -l
```

The `capture` command records both to one synchronized Matroska file
natively, with no external FFmpeg process and no independent per-input
clocks:

```sh
target/release/handycam capture \
  --output handycam-record.mkv \
  --alsa-device hw:1,0 \
  --semantic-init --quality 20
```

Use `--playback-init` instead of `--semantic-init` for tape playback, and
start transport from another terminal as usual. See
[A/V synchronization next steps](docs/next-steps-av-sync.md) for how this
is implemented and what is still only validated by hardware-free tests.

V4L2 and raw MJPEG stdout carry video only. The external-FFmpeg workflow
below remains available -- for example to feed `ffplay` directly, or on a
host where `capture`'s ALSA integration hasn't been validated yet -- but its
two inputs do not share a clock, so calibration is needed; see "Calibrating
a fixed audio offset" in the [production driver guide](docs/production-driver.md).
To capture a multiplexed file this way, use the video pipe and ALSA as two
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

This external pipeline's audio and video clocks are independent, so a small
fixed offset may be needed for a particular host. See "Calibrating a fixed
audio offset" in the [production driver guide](docs/production-driver.md) for
a measurement procedure -- or use `capture` above, which does not have this
problem.

## Documentation

- [Production driver guide](docs/production-driver.md)
- [A/V synchronization next steps](docs/next-steps-av-sync.md)
- [Browser-based monitoring next steps](docs/next-steps-webusb-monitoring.md)
- [Protocol and reverse-engineering record](reverse-engineering/PROTOCOL.md)
- [Reverse-engineering evidence and experiments](reverse-engineering/)

The reverse-engineering directory contains the preserved traces, driver
images, VM notes, analyzers, and historical implementation. It is deliberately
separate from the production Rust path.
