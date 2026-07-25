# Rust production-driver validation

Date: 2026-07-24  
Camera: Sony DCR-HC24, `054c:00c0`, live record mode  
Host: Ubuntu 26.04, Linux 7.0.0-27 and 7.0.0-28

## Automated

`make rust-check` passes:

- rustfmt check;
- Clippy across all targets with warnings denied;
- 12 unit/regression tests across the workspace;
- replay of the complete first direct-Linux capture when the research corpus
  is present;
- byte-exact JPEG reconstruction against the Python oracle;
- locked YUYV output hash; and
- `handycam-core` build for `wasm32-unknown-unknown`.

The embedded initialization plan test verifies 223 operations, 140 writes,
80 reads, alt 0/7/5, and 2,563,101 microseconds of recorded delay.

## Live stdout: MJPEG

The Rust libusb backend initialized the camera at USB path `001-2` and wrote
standard concatenated JPEG frames to stdout.

A bounded run produced:

```text
108 frames
320x240
25/1 fps
740,282 bytes
```

FFprobe recognized MJPEG and FFmpeg decoded the entire file without errors.
Closing the downstream pipe after one byte also caused a clean exit after
cancelling and reaping transfers.

## Live stdout: YUYV

The decoded-output run produced:

```text
132 frames
153,600 bytes per frame
20,275,200 bytes total
zero trailing bytes
320x240 yuyv422 at 25/1 fps
```

FFmpeg consumed the complete raw stream without errors.

## Sustained transport run

A 65-second process run included roughly 60 seconds after initialization:

```text
USB packets:        120,012
nonempty packets:    15,018
USB bytes:        9,372,690
boundaries:            1,502
headers:               1,501
emitted frames:        1,499
packet errors:             0
protocol errors:           0
output queue drops:        0
```

There was one valid 41 ms camera timestamp step; all other observed steps in
the run were 40 ms. The driver reported the cadence variation and retained
the frame.

SIGINT cleanup returned status zero.

## V4L2 loopback integration

Secure Boot initially rejected the DKMS module. The existing
`/var/lib/shim-signed/mok/MOK.der` certificate matched the module signer and
was enrolled through MOK Manager. `v4l2loopback` 0.15.3 then created
`/dev/video10` with output, read/write, and streaming capabilities.

The new `handycam replay` command replayed the 124 reconstructed JPEGs from
the direct-Linux capture through the production V4L2 sink. FFmpeg consumed
100 frames from each format:

```text
MJPEG: 100/100 packet hashes exactly matched source JPEGs
       320x240, 25/1 fps, clean decode
YUYV:  100/100 frame hashes exactly matched replay conversion
       15,360,000 bytes, 153,600 bytes/frame, clean decode
```

The first MJPEG run exposed a memory-mapped output-buffer lifetime bug: the
payload came from frame N+1 while `bytesused` came from frame N. Packet sizes
looked plausible and FFmpeg tolerated most frames, but hashes did not match.
Changing the V4L2 sink to one `write(2)` call per frame eliminated the skew;
all subsequent packet and frame hashes matched byte-for-byte.

With no camera attached, the production V4L2 command configured the output
and retried device discovery at 500 ms, 1 s, and 2 s intervals before a
SIGINT cleanly returned status zero. A looping replay whose stdout consumer
closed after one byte also returned status zero.

The example USB udev rule passes `udevadm verify`. The systemd example parses
but naturally reports its `/usr/local/bin/handycam` executable as missing
until the binary is installed there.

## Pending live acceptance

Live camera-to-V4L2 capture and a physical unplug/replug acceptance run
remain pending until the camera can be connected again. Live camera-to-stdout
and recorded-source-to-V4L2 have both passed independently.
