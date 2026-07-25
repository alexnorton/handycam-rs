# Production userspace driver

The Rust implementation captures live record-mode video directly from a Sony
DCR-HC24 and emits either an MJPEG stream, raw YUYV frames, or a V4L2 virtual
camera. It does not use the Windows VM or Sony driver.

## Build

Install a stable Rust toolchain, `libusb-1.0` development files, Clang, and
the V4L2 headers. The optional V4L2 path also needs `v4l2loopback`; stdout
does not. The `handycam-alsa` crate links against `libasound2-dev` (via
`pkg-config`); this is required to build the workspace at all, even before a
`capture` command exists to use it.

```sh
. "$HOME/.cargo/env"
make rust
```

The binary is `target/release/handycam`. The complete automated check is:

```sh
make rust-check
```

Install only the unprivileged executable with:

```sh
sudo make install
```

`PREFIX` and `DESTDIR` follow their conventional Make meanings, so package
staging can use, for example, `make install DESTDIR=/tmp/handycam-package`.
Kernel-module, udev, and service configuration remains explicitly opt-in
under `contrib/`.

## Stdout

MJPEG is the default and preserves the camera's compressed stream after
adding standard JPEG headers and byte stuffing:

```sh
target/release/handycam stream \
  --output - \
  --semantic-init \
  --quality 20 |
  ffplay -f mjpeg -framerate 25 -
```

To write a file through FFmpeg:

```sh
target/release/handycam stream --output - --semantic-init --quality 20 |
  ffmpeg -f mjpeg -framerate 25 -i - -c:v copy handycam.mkv
```

### Native synchronized capture

`capture` owns both the USB video session and an ALSA audio thread and writes
one Matroska file with both tracks on a shared clock:

```sh
target/release/handycam capture \
  --output handycam-record.mkv \
  --alsa-device hw:1,0 \
  --semantic-init --quality 20
```

Use `--playback-init` instead of `--semantic-init` for tape playback, and
start transport from another terminal with `handycam transport play`. This
is implemented and unit-tested but not yet validated against real camera and
ALSA hardware; see [A/V synchronization next steps](next-steps-av-sync.md)
for what that validation involves and its current status.

### Capturing audio with the video pipe (FFmpeg fallback)

The driver can also emit video on stdout alone, leaving the camera's audio
interfaces available to ALSA directly. Find the device with `arecord -l`,
then mux both inputs with FFmpeg:

```sh
target/release/handycam stream --output - --semantic-init --quality 20 |
ffmpeg \
  -thread_queue_size 512 -f mjpeg -framerate 25 -i pipe:0 \
  -thread_queue_size 512 -f alsa -ar 16000 -ac 2 -i hw:1,0 \
  -map 0:v:0 -map 1:a:0 -c:v copy -c:a pcm_s16le \
  -af aresample=async=1 handycam-record.mkv
```

For tape playback, use `--playback-init` instead of `--semantic-init` and
start transport from another terminal with `handycam transport play`.
Unlike `capture`, this pipeline's ALSA and camera clocks are independent and
can produce a small offset; use FFmpeg's `-itsoffset` on the audio input when
calibrating a recording, or prefer `capture` above.

### Calibrating a fixed audio offset

The FFmpeg pipeline above has no shared clock between the video pipe and the
ALSA input, so recordings can show a small, host-dependent fixed offset.
Measure it once per host/device combination rather than guessing:

1. Record a short clip containing a single sharp, visually and audibly
   distinct event — a clap, a bell, or a camera flash works well:

   ```sh
   target/release/handycam stream --output - --semantic-init --quality 20 |
   ffmpeg \
     -thread_queue_size 512 -f mjpeg -framerate 25 -i pipe:0 \
     -thread_queue_size 512 -f alsa -ar 16000 -ac 2 -i hw:1,0 \
     -map 0:v:0 -map 1:a:0 -c:v copy -c:a pcm_s16le \
     -af aresample=async=1 calibration.mkv
   ```

2. Find the event's timestamp in each stream. `ffprobe -show_frames` lists
   per-frame `pts_time`; visually identify the video frame containing the
   event, and separately inspect the audio waveform (for example in
   Audacity, or `ffprobe -f lavfi "amovie=calibration.mkv,astats=metadata=1"`)
   to find the sample where the clap/flash's audio onset occurs.

3. The difference between the two (`audio_event_time - video_event_time`) is
   the offset to apply. A positive value means audio arrived late relative to
   video and should be shifted earlier (or the video delayed); apply it to
   the *next* recording's audio input, never as a default:

   ```sh
   ffmpeg \
     -thread_queue_size 512 -f mjpeg -framerate 25 -i pipe:0 \
     -itsoffset 0.25 -thread_queue_size 512 -f alsa -ar 16000 -ac 2 -i hw:1,0 \
     -map 0:v:0 -map 1:a:0 -c:v copy -c:a pcm_s16le \
     -af aresample=async=1 handycam-record.mkv
   ```

4. Repeat the measurement at 30 seconds, 5 minutes, and 30 minutes into a
   longer recording. A consistent offset across all three means the fixed
   startup skew dominates and `-itsoffset` alone is sufficient. A growing gap
   means the ALSA and camera clocks are drifting relative to each other;
   `aresample=async=1` only smooths small differences and cannot correct a
   fixed offset, so the two problems need to be diagnosed separately.

This calibration is a stopgap. It is host- and device-specific, must be
redone if the USB topology or audio device changes, and does not correct
drift. The full fix is native timestamp plumbing, tracked in
[A/V synchronization next steps](next-steps-av-sync.md).

For applications that need uncompressed frames:

```sh
target/release/handycam stream \
  --output - \
  --format yuyv \
  --semantic-init \
  --quality 20 |
  ffmpeg \
    -f rawvideo \
    -pixel_format yuyv422 \
    -video_size 320x240 \
    -framerate 25 \
    -i - \
    handycam.mkv
```

Stdout contains frame bytes only. Logs always go to stderr. Closing a
downstream pipe stops the camera cleanly.

## Tape playback and transport

In playback mode, use the playback-specific startup acknowledgement:

```sh
target/release/handycam stream \
  --output - \
  --playback-init \
  --quality 20 |
ffplay -f mjpeg -framerate 25 -i -
```

The recovered controls can inspect status or send one sequenced operation:

```sh
target/release/handycam transport status
target/release/handycam transport play
target/release/handycam transport pause
target/release/handycam transport stop
```

Fast-forward and Rewind are available as `fast-forward` and `rewind`. The
current interface is explicitly experimental. By default it derives the
current sequence from the high nibble of status byte 0, increments it, and
wraps after 15. `--current-sequence` remains available for exact protocol
experiments. The command reports the exact word, status before/after, and
every changed status byte. It waits 500 ms by default so the physical state
in byte 2 has time to settle; override that only for protocol timing
experiments.

Transport transitions can yield a single MJPEG entropy-concealment warning in
FFmpeg even when USB framing remains intact. Pillow decoded every frame in
the repeated validation runs, so consumers should tolerate a recoverable
tape-transition frame.

Record-mode `--semantic-init` waits for `n1` startup acknowledgements.
Playback-mode `--playback-init` waits for the observed `n9` acknowledgements.
Literal initialization remains available by omitting both flags.

## V4L2 virtual camera

The driver deliberately does not run privileged commands or manage kernel
modules. On Ubuntu, install the loopback packages:

```sh
sudo apt install v4l2loopback-dkms v4l2loopback-utils v4l-utils
sudo modprobe v4l2loopback \
  video_nr=10 \
  card_label="Sony DCR-HC24" \
  exclusive_caps=1
```

Then run:

```sh
target/release/handycam stream \
  --output /dev/video10 \
  --semantic-init \
  --quality 20
```

If `modprobe` reports `Key was rejected by service`, Secure Boot has rejected
the DKMS module's signing certificate. Check whether the certificate that
signed the module is already enrolled:

```sh
modinfo -F signer v4l2loopback
sudo mokutil --test-key /var/lib/shim-signed/mok/MOK.der
```

On Ubuntu, enroll the existing DKMS Machine Owner Key with:

```sh
sudo mokutil --import /var/lib/shim-signed/mok/MOK.der
```

Choose a temporary enrollment password when prompted, reboot, and select
**Enroll MOK**, **Continue**, and **Yes** in the blue MOK Manager screen.
Enter the temporary password and reboot once more. Then verify and load the
module:

```sh
mokutil --test-key /var/lib/shim-signed/mok/MOK.der
sudo modprobe v4l2loopback \
  video_nr=10 \
  card_label="Sony DCR-HC24" \
  exclusive_caps=1
```

Use `--format yuyv` if a consumer does not accept MJPEG. Confirm the device
with:

```sh
v4l2-ctl --device /dev/video10 --all
ffplay -f v4l2 -framerate 25 -video_size 320x240 /dev/video10
```

With `exclusive_caps=1`, start the Handycam producer first. The loopback node
advertises capture capability only after the producer supplies its first
frame; a consumer opened earlier reports `Not a video capture device`.

## Recorded-frame replay

The replay command drives the production stdout or V4L2 sink from one
reconstructed JPEG or a filename-sorted directory of JPEGs. It runs at the
camera's 25 fps cadence and is useful for testing consumers without the
camera:

```sh
target/release/handycam replay \
  --input reverse-engineering/captures/frames-linux-init-first-v2 \
  --output /dev/video10 \
  --loop
```

Add `--format yuyv` to test the uncompressed sink. Interrupt a looping replay
with Ctrl-C. Replay bypasses USB and Sony framing, so it validates output
integration but does not replace the live-camera acceptance test.

For a stronger no-camera regression, replay a boundary-preserving endpoint
extraction through the production Sony decoder:

```sh
target/release/handycam replay-capture \
  --input reverse-engineering/captures/live-linux-init-first \
  --output - > /tmp/replayed.mjpg
```

This reads `iso-packets.jsonl`, `ep81.bin`, and `ep82.bin`; it exercises
cross-endpoint framing, JPEG reconstruction, and the selected output sink.
The reference capture produces 123 frames with no protocol errors.

The files under `contrib/` are opt-in examples for persistent module
configuration, USB permissions, and a system service. Review paths, user,
group, and video node before copying them into `/etc`.

## Optional persistent service

After reviewing the example files and completing any Secure Boot enrollment,
a conventional system installation is:

```sh
sudo make install
sudo useradd \
  --system \
  --no-create-home \
  --home-dir /nonexistent \
  --shell /usr/sbin/nologin \
  --groups video \
  handycam
sudo install -Dm644 contrib/99-handycam.rules \
  /etc/udev/rules.d/99-handycam.rules
sudo install -Dm644 contrib/v4l2loopback.conf \
  /etc/modprobe.d/handycam-v4l2loopback.conf
sudo install -Dm644 contrib/v4l2loopback.modules-load.conf \
  /etc/modules-load.d/handycam-v4l2loopback.conf
sudo install -Dm644 contrib/handycam.service \
  /etc/systemd/system/handycam.service
sudo udevadm control --reload
sudo systemctl daemon-reload
sudo systemctl enable --now handycam.service
```

If the `handycam` account already exists, inspect its group membership rather
than rerunning `useradd`. Replug the camera after installing the udev rule.
The service waits for an absent camera and reconnects automatically.

## Device permissions

For a one-connection test, grant an ACL to the camera's current USB node as
shown in the main README. For persistent use, install a reviewed version of
`contrib/99-handycam.rules`, add the service user to the `video` group, and
reload udev rules.

The process claims only vendor interface 0. Linux continues to expose the
camera's standard audio interfaces through `snd-usb-audio`.

`--semantic-init` replaces four fixed-length startup status-read runs with
real acknowledgement polling. It is live-validated and reduces initialization
from roughly 2.66 seconds to about 1.05 seconds on this camera. Omit it to
retain exact literal replay as a diagnostic fallback.

`--quality` selects the startup JPEG scale factor from 4 through 128. Larger
values produce smaller, lower-quality frames. Values 4, 20, 40, 80, and 128
have all passed live capture and decode tests. Runtime quality switching is
not yet exposed.

## Runtime behavior

The program waits when the camera is absent and automatically reinitializes
it after reconnection. Use `--no-reconnect` for one-shot operation.

If more than one matching camera is attached, select its physical USB path:

```sh
target/release/handycam stream \
  --usb-path 001-2.3 \
  --output /dev/video10
```

The path uses the USB bus followed by the stable port chain, not the
short-lived USB device address.

Diagnostics include USB packet counts, frame boundaries, record headers,
timestamp gaps, dropped output frames, and protocol errors. Increase detail
with `-v`, or select structured stderr logs with `--log-format json`.

On shutdown, every asynchronous transfer is cancelled and reaped before
interface 0 returns to alt 0.

## Architecture

The Cargo workspace separates:

- `handycam-core`: a safe, platform-neutral state machine, initialization
  representation, JPEG reconstruction, YUYV conversion, and the
  `Synchronizer` that anchors video and audio timestamps to one shared
  presentation timeline;
- `handycam-libusb`: the native USB transport and its narrowly contained
  unsafe libusb transfer lifecycle;
- `handycam-alsa`: timestamped ALSA audio capture, using hardware capture
  timestamps when available and a monotonic-read-time estimate otherwise;
- `handycam-mkv`: a minimal streaming EBML/Matroska writer for one MJPEG
  video track and one PCM audio track; and
- `handycam-cli`: Linux stdout/V4L2 sinks, the native `capture` muxing path,
  and process supervision.

The V4L2 sink uses one `write(2)` call per complete frame. This preserves the
variable byte count of each MJPEG frame and avoids reusing a memory-mapped
output buffer before a loopback consumer has finished with it.

`handycam-core` builds for `wasm32-unknown-unknown`. A future WebUSB backend
can provide individual endpoint packets to the same `StreamDecoder`.

The platform-neutral core contains a typed, tested encoder for Play, Pause,
Stop, Fast-forward, and Rewind recovered from Sony's original application.
All five operations, status transitions, repeated commands, and sequence
wraparound are live-validated. The Linux CLI exposes them through an
experimental one-shot command while a persistent stateful control API remains
future work.

Native USB packet events also carry a host monotonic timestamp captured at
libusb callback entry. This is the clock handoff the `capture` command's
`Synchronizer` uses to correlate video against `handycam-alsa`'s own capture
timestamps; the `stream` V4L2 and stdout sinks still don't need it and
continue to ignore it.
