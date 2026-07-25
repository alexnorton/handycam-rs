# Offline control, quality, and A/V analysis

Date: 2026-07-24  
Camera required: no

This pass used the preserved Windows/OHCI traces, the extracted isochronous
packets, and `sonypvs1.sys`. Its purpose was to make progress on semantic
initialization, quality selection, transport infrastructure, synchronized
audio, and regression testing while the camera was unavailable.

## Control transcript

`extract_sony_control.py` stages a pcap under `/tmp` for tshark, pairs each
control submission with its completion URB, and retains IN response data:

```sh
../tools/extract_sony_control.py \
  ../captures/usb-record-mode-ohci.pcapng \
  /tmp/ohci-control.jsonl \
  --device-address 16

../tools/analyze_sony_control.py \
  /tmp/ohci-control.jsonl \
  > /tmp/ohci-control-analysis.json
```

Source:

```text
../captures/usb-record-mode-ohci.pcapng
SHA-256 f4642d1197e85b962076bb0af99bf100aa0d33bc2071fbd3e848ddfcfec5c02d
```

Extraction:

```text
control transactions:       7,830
Sony request-0x88:          7,820
vendor reads:               7,006
vendor writes:                824
unmatched submissions:          0
```

### Status buffer

There are 6,984 complete reads of the 64-byte buffer at `0x0340`, containing
136 unique values. Bytes 10–63 are always zero. The changing part is:

```text
byte 0: 01, 11, 21, …, f1
byte 4: 12, 52, 92, d2
byte 8: 12, 52, 92, d2
byte 9: 00 or 10
```

Bytes 4 and 8 are equal in 6,976 of 6,984 reads. Every mismatch has byte 4
one step ahead of byte 8 in the four-state cycle. These look like producer
and consumer cursors, although that name remains an inference.

### Command mailbox

The 523 writes to `0x0300` divide into:

```text
342 × 00 n9 10 n1
177 × 00 n1 18 n0
  4 × 00 18 80 10
```

For the first family, status byte 0 acknowledges the command's final byte.
305/342 commands have that acknowledgement before the next mailbox write.
Latency for those matched commands:

```text
minimum:    524 µs
median:  65,193 µs
p95:    100,142 µs
maximum: 3,054,219 µs
```

Some unmatched cases are immediate back-to-back writes without an
intervening status read. During initialization the sequence is especially
clear:

| Command frame | Command | First matching status frame |
|---:|---|---:|
| 4522 | `00 79 10 71` | 4528 |
| 4556 | `00 89 10 81` | 4564 |
| 4586 | `00 99 10 91` | 4592 |
| 4620 | `00 a9 10 a1` | 4626 |

This gives a condition-based replacement for the startup trace's fixed
status-read counts.

## Initialization annotation

`ToDeviceCaptureStart` calls the device-manager routines in this order:

```text
Init → Estimation → EnableESFeatures → DisableSDRAMBuffering →
WriteReg07E → TurnOffVideo → SetColorSpace → SetRes → SetFrameRate →
WriteQuantizationMatrices → WriteScalingFactors → SetBanding →
Handshake → SetCodeMode → ResetEncoder → TurnOnVideo
```

The named static tables still provide exact matches for PowerOff, AudioOn,
Init, and PowerOn. `annotate_sony_init.py` merges those exact matches with
the recovered call order:

```sh
../tools/annotate_sony_init.py \
  ../../crates/handycam-core/assets/record-mode-init.tsv \
  /tmp/record-mode-init-annotated.tsv
```

All 223 plan operations receive one of 20 phase labels. Confidence is
explicit in the output. In particular, frames 4494–4520 remain a composite
encoder-start phase rather than pretending that the boundaries between
Handshake, SetCodeMode, ResetEncoder, and TurnOnVideo have been proved.

## Quality comparison

The low-quality pcap contains more than one Sony address. Address 16 is the
capture session used here:

```sh
../tools/extract_sony_control.py \
  ../captures/usb-record-mode-low-quality-piix3.pcapng \
  /tmp/lowq-control.jsonl \
  --device-address 16

../tools/compare_sony_quality.py \
  /tmp/ohci-control.jsonl \
  /tmp/lowq-control.jsonl
```

Normal-quality initialization has two observed reset/configure pairs:

```text
0x10 → 0x14 (20)
```

Subsequent dynamic values descend as low as 9. Low-quality calibration has
four observed reset/configure pairs:

```text
0x10 → 0x50 (80)
```

Its subsequent values range from 78 through 90. The luma and chroma matrix
payload hashes are identical across both traces:

```text
0x0080  672e8b191bd158a305af35cbe95ec02ebb47e64099f14e7bdf4ef69f933d9161
0x00c0  d43fce8d732ba8f0e1132402439a2d1b7fe98fc65a49deff867f6860dc6d8810
```

The low-quality stream contains 3,549 bounded records with a median raw span
of 1,824 bytes. This establishes that `0x007c`, rather than an alternate DQT
upload, is the independent quality control on this camera.

## Offline synchronized A/V

`build_sony_av.py` selects a continuous video session, bounds each `0x82`
record at the next descriptor-start header, reconstructs and validates every
JPEG, and anchors `0x83` PCM to the nearest unwrapped one-millisecond USB
frame:

```sh
../tools/build_sony_av.py \
  ../captures/extracted-record-mode-ohci-all \
  /tmp/sony-av
```

The longest session has 193 headers and therefore 192 bounded frames:

```text
video:        MJPEG, 320x240, 192 USB-timed frames
audio:        PCM S16LE, stereo, 16,000 Hz, 144,640 sample frames
duration:     9.040 seconds
USB anchors:  video 166243, audio 166243
unique JPEGs: 192
```

Its source timestamp delta histogram is:

```text
0 ms:   155
40 ms:   27
80 ms:    4
120 ms:   3
160 ms:   2
```

The Matroska result remains synchronized because the mux assigns every video
PTS from its unwrapped USB frame and takes the matching audio duration from
the counted 64-byte/ms PCM stream. Missing or delayed video frames therefore
remain time gaps instead of being squeezed into a false constant-rate
7.680-second sequence. This validates the production clock priority before
ALSA integration.

Validation artifact from this run:

```text
capture.mkv
SHA-256 8f2167e455935c2bc2921f4db99c9d452fecb3523d03e8d727be7459cb599dae
```

The generated media remains under `/tmp`; it is reproducible and is not part
of the source tree.

## Raw-capture regression backend

The Rust CLI now has `replay-capture`, which differs from JPEG replay by
feeding preserved descriptor packets through the production `StreamDecoder`
and sink:

```sh
cargo build -p handycam-cli --locked
target/debug/handycam replay-capture \
  --input ../captures/live-linux-init-first \
  --output - \
  > /tmp/replayed.mjpg
```

Result:

```text
input packets:    1,455
output frames:      123
protocol errors:      0
ffprobe:          MJPEG, 320x240, 25 fps, 123 frames
SHA-256:          4dde8be027018b1b306648eacc3e0cdd447ba6fa66fd31153edd0f49846d7fdf
```

This is the no-camera regression path for changes to framing, reconstruction,
and stdout/V4L2 output.

## Next camera-dependent experiments

The remaining highest-value work requires the hardware:

1. capture one playback action per trace to map play, pause, stop, rewind,
   fast-forward, and status transitions;
2. replace the startup scan reads with `status[0]` polling and confirm a
   five-second capture plus reconnect and soak;
3. apply fixed scale factors 4, 20, 40, 80, and 128 to a fixed scene, then
   test whether a single live `0x007c` write is safe;
4. add monotonic/USB timing to the native video transport and align it with
   ALSA sample timestamps.
