# Sony DCR-HC24 USB video protocol

Status: implemented and live-validated for record capture and primary playback transport
Device tested: Sony DCR-HC24  
USB identity: `054c:00c0`  
USB speed: full speed, 12 Mbit/s

This document describes the vendor-specific USB video protocol sufficiently
to initialize the camera, receive its compressed video stream, identify
frames, and turn those frames into standard JPEG images. It is based on USB
descriptors, successful and failed usbmon traces, disassembly of Sony's
`sonypvs1.sys` Windows XP driver, and a working Linux libusb implementation.

The production implementation is the Rust workspace described in
[`../docs/production-driver.md`](../docs/production-driver.md). The original C and
Python programs remain useful as evidence-oriented capture and analysis
tools.

Commands in this document that use `captures/`, `artifacts/`, or `./tools/`
are run from this `reverse-engineering/` directory. The old C prototype is
kept under `legacy/` for historical comparison; new captures should use the
Rust binary documented in the production guide.

The protocol documented here is the camera's live record-mode stream. Tape
playback control and other Sony models may use additional operations.

## Protocol status

The following are demonstrated, not inferred:

- direct Linux initialization using control request `0x88`;
- continuous isochronous input on endpoints `0x81` and `0x82`;
- frame synchronization at 25 frames per second;
- reconstruction of individual compressed records;
- decoding as 320x240 baseline JPEG with 4:2:0 sampling; and
- simultaneous ownership of the standard audio interfaces by
  `snd-usb-audio`.

The exact meaning of most Sony register indices is not yet known. The current
implementation replays a known-good initialization sequence rather than
claiming semantic knowledge of every register.

## USB layout

The device has one configuration and three interfaces.

| Interface | Class | Purpose |
|---:|---|---|
| 0 | Vendor-specific | Sony video initialization, status, and streaming |
| 1 | USB Audio Control | Standard audio topology |
| 2 | USB Audio Streaming | Standard PCM input |

### Interface 0: vendor video

Interface 0 has alternate settings 0 through 7. Every active alternate has
two isochronous IN endpoints with a one-millisecond interval.

| Alternate | `0x81` maximum | `0x82` maximum |
|---:|---:|---:|
| 0 | 0 | 0 |
| 1 | 8 | 256 |
| 2 | 8 | 384 |
| 3 | 8 | 512 |
| 4 | 8 | 640 |
| 5 | 8 | 768 |
| 6 | 8 | 896 |
| 7 | 8 | 1023 |

The verified stream uses alternate 5. Sony's Windows driver first tries
alternate 7 and then selects alternate 5. Selecting alternate 5 by itself is
not sufficient to start video: both endpoints return successful zero-length
ISO packets until the Sony initialization transactions have run.

Endpoint roles:

- `0x81` carries short synchronization/status packets. Active packets in the
  observed stream are five bytes even though the descriptor permits eight.
- `0x82` carries the compressed video record.

### Interfaces 1 and 2: standard audio

Interface 1 is a conventional USB Audio Control interface. Interface 2 alt 1
provides:

```text
endpoint:       0x83 isochronous IN
channels:       2
sample format:  signed 16-bit little-endian PCM
sample rate:    16,000 Hz
maximum packet: 64 bytes per USB frame
```

No Sony-specific audio driver is required on Linux. Interface 0 can be
claimed through libusb while `snd-usb-audio` retains interfaces 1 and 2.

## Control protocol

Sony uses vendor request `0x88` on endpoint zero.

### Register-style read

```text
bmRequestType = 0xc0  (device-to-host, vendor, device)
bRequest      = 0x88
wValue        = 0
wIndex        = register or buffer index
wLength       = requested byte count
```

### Register-style write

```text
bmRequestType = 0x40  (host-to-device, vendor, device)
bRequest      = 0x88
wValue        = 0
wIndex        = register or buffer index
wLength       = payload byte count
data          = value or block to write
```

The known-good record-mode plan is
[`../crates/handycam-core/assets/record-mode-init.tsv`](../crates/handycam-core/assets/record-mode-init.tsv). It contains
223 retained operations over 2.563101 seconds:

- 140 vendor writes;
- 80 vendor reads; and
- three standard `SET_INTERFACE` requests for interface 0: alt 0, alt 7,
  then alt 5.

Of the writes, 129 contain one byte and 11 contain 64 bytes. Of the reads, 77
request 64 bytes and three request one byte.

Frequently observed indices include:

| Index | Direction/length | Observed role |
|---:|---|---|
| `0x0340` | IN, 64 bytes | Repeated status/state polling |
| `0x0300` | OUT, 64 bytes | Repeated state/control block updates |
| `0x0080` | OUT, 64 bytes | Transposed luminance quantization matrix |
| `0x00c0` | OUT, 64 bytes | Transposed chrominance quantization matrix |

The descriptions of `0x0340` and `0x0300` are intentionally generic. Later
sections document demonstrated field correlations without assigning
unproved semantic register names.

### Recovered initialization phases

The driver's CodeView symbols expose the register tables used by its named
device-manager routines. `analyze_sony_driver.py` maps those virtual
addresses back into the PE image and compares the tables with the captured
plan. Four routines match exactly:

| Routine | Capture frames | Writes |
|---|---:|---|
| `DevmanPowerOff` | 4202–4204 | `35=06`, `35=00` |
| `DevmanAudioOn` | 4256–4262 | `23=00`, `24=68`, `22=0c`, `35=0f` |
| `DevmanInit` | 4334–4378 | 21-register core initialization |
| `DevmanPowerOn` | 4380–4386 | `35=06`, `36=01`, `36=03`, `35=0f` |

`DevmanPowerOn` waits 10 ms between its second and third writes. The static
`DevmanInit` table differs from the trace in only two runtime-populated
values: index `0x61` is `0x3f` rather than `0x20`, and index `0x51` is `0x03`
rather than `0x00`.

Run the reproducible extraction with:

```sh
python3 tools/analyze_sony_driver.py |
  jq '{register_sequences, initialization_plan_matches}'
```

These names let the replay be decomposed phase by phase while retaining the
exact TSV as a fallback.

Disassembly of `ToDeviceCaptureStart` also establishes this call order after
the four named-table phases:

```text
DevmanInit
DevmanEstimation(1, 1)
DevmanEnableESFeatures
DevmanDisableSDRAMBuffering
DevmanWriteReg07E
DevmanTurnOffVideo
DevmanSetColorSpace
DevmanSetRes
DevmanSetFrameRate
DevmanWriteQuantizationMatrices
DevmanWriteScalingFactors
DevmanSetBanding(0)
DevmanHandshake
DevmanSetCodeMode
DevmanResetEncoder
DevmanTurnOnVideo
```

`annotate_sony_init.py` combines that ordering with exact static-table
matches and emits an expanded TSV. Each label carries a confidence field;
the handshake/code/reset/video-on tail deliberately remains one composite
phase until its internal boundaries can be proved:

```sh
./tools/annotate_sony_init.py \
  ../crates/handycam-core/assets/record-mode-init.tsv \
  /tmp/record-mode-init-annotated.tsv
```

### Command mailbox and Apollo registers

Sony's `CustomPropCommandWrite` creates a zeroed 64-byte buffer, stores a
32-bit command in big-endian order at offsets 60–63, and writes the block to
index `0x0300`:

```text
00 ... 00 | command[31:24] command[23:16] command[15:8] command[7:0]
<---------------- 64 bytes ----------------------------------------->
```

The full record-mode control transcript exposes two recurring command
families. Here `n` is a four-bit sequence nibble:

```text
scan:    00 n9 10 n1
paired:  00 n1 18 n0
special: 00 18 80 10
```

The scan family has mode-specific acknowledgement rules after command
`00 n9 10 n1`:

- record/camera mode completes when status byte 0 equals `n1`;
- tape playback mode acknowledges with status byte 0 equal to `n9`.

For example, record-mode initialization sends commands ending in `71`, `81`,
`91`, and `a1`;
the first matching status bytes occur at capture frames 4528, 4564, 4592,
and 4626. Across the full trace, 305 of 342 scan commands were acknowledged
before the next mailbox write. Median acknowledgement latency was 65.193 ms
and the 95th percentile was 100.142 ms. The remaining cases include
back-to-back commands with no intervening status poll, so they do not
contradict the rule.

Live playback-mode polling observed `79`, `89`, `99`, and `a9` after 6, 5,
5, and 5 polls. Both modes can therefore replace the opaque fixed count of
status reads with a bounded, mode-specific condition poll.

### Playback transport commands

Static analysis of the preserved Sony application recovered the complete
primary transport path without requiring a running XP driver:

```text
CapturingTool button handler
  -> USB01 input plug-in
  -> S2UCapture!_S2UCommand
  -> IKsPropertySet property 0x15
  -> sonypvs1!CustomPropCommandWrite
  -> request 0x88, index 0x0300
```

The application resource dialog names the five controls, and their handlers
pass the following fixed command bytes:

| Operation | Sony command byte |
|---|---:|
| Play | `0x1a` |
| Pause | `0x19` |
| Stop | `0x18` |
| Fast-forward | `0x1c` |
| Rewind | `0x1b` |

`S2UCapture` does not place this byte in the mailbox alone. It increments a
four-bit counter modulo 16, duplicates that sequence nibble, inserts custom
property selector `1`, and constructs:

```text
00 n1 cc n0
```

Here `cc` is the operation byte and `n` is the newly incremented sequence.
For example, Play with sequence `1` is `00 11 1a 10`; the next Pause is
`00 21 19 20`. The resulting 32-bit value is sent through property `0x15`.
The kernel driver's `CustomPropCommandWrite` converts that value to big
endian at offsets 60–63 and writes the 64-byte block to `0x0300`.

This independently explains the record-mode paired family
`00 n1 18 n0`: `0x18` is Stop. The Rust core implements the five typed
operations and this wrapping encoder.

Live Linux validation confirms every primary operation and supplies the
transport-state value in status byte 2:

| Operation | Command byte | Status byte 2 |
|---|---:|---:|
| Stop | `0x18` | `0x02` |
| Play | `0x1a` | `0x06` |
| Pause | `0x19` | `0x07` |
| Fast-forward | `0x1c` | `0x03` |
| Rewind | `0x1b` | `0x83` |

Status byte 0 echoes the command sequence in its high nibble. Repeated Stop
commands advanced acknowledgements through `b9`, `c9`, `d9`, `e9`, `f9`,
then `09`, proving modulo-16 hardware wraparound. During stable Play/Pause,
the low nibble can settle from `9` to `1`; the high nibble remains the
transaction correlation field.

Play produced non-silent tape audio, Pause produced digital silence, and a
concurrent video capture changed from one frozen stopped image to 87 distinct
decoded motion frames before freezing on Pause and returning to the original
image on Stop.

`extract_sony_control.py` reconstructs control submissions and completions
without losing IN response data, and `analyze_sony_control.py` produces the
command/status evidence:

```sh
./tools/extract_sony_control.py captures/usb-record-mode-ohci.pcapng \
  /tmp/ohci-control.jsonl --device-address 16
./tools/analyze_sony_control.py /tmp/ohci-control.jsonl
```

The driver's four logical Apollo registers map to Sony request indices
`0x2b`, `0x2c`, `0x2d`, and `0x2e`. Frame-rate selection writes logical
Apollo register 0, hence physical index `0x2b`.

### Status block at `0x0340`

The successful OHCI trace contains 6,984 complete 64-byte status reads and
136 unique buffers. Only bytes 0 through 9 ever carry nonzero or changing
data:

| Byte | Observed values | Evidence-based interpretation |
|---:|---|---|
| 0 | `01, 09, 11, 19, …, f1, f9` | command sequence/acknowledgement token |
| 1 | always `01` | invariant, meaning unknown |
| 2 | `02, 03, 06, 07, 83` | Stop, FF, Play, Pause, Rewind transport state |
| 4 | multiple values, normally equals byte 8 | first mirrored dynamic state/cursor |
| 6 | `03` in record trace; `04/05` during playback tests | playback-position/status field |
| 7 | `52` in record trace; changes with tape motion | playback-position/status field |
| 8 | multiple values, normally equals byte 4 | second mirrored dynamic state/cursor |
| 9 | `00` or `10` | toggling state bit, meaning unknown |
| 10–63 | always zero | unused in this trace |

Bytes 4 and 8 were equal in 6,976/6,984 reads. In the eight transient
mismatches, byte 4 was exactly one state ahead in the cycle
`12 → 52 → 92 → d2 → 12`. “Producer and consumer cursor” is therefore a
strong working model, but not yet a proven register name.

The current playhead position is not yet decoded as a human-readable time.
Bytes 6–7 change during tape motion and are the strongest position/timecode
candidate, but we have not correlated them with the camera LCD or a known
tape time. No clip capture date or date/time metadata has appeared in the
64-byte status block, reconstructed JPEG headers, or the observed control
responses. It may instead live in tape DV subcode or an unobserved metadata
request.

Mode changes are exposed by polling, not a discovered notification mechanism.
The vendor interface has only isochronous IN endpoints `0x81`/`0x82` across
its alternate settings; status is returned by repeated endpoint-zero IN reads
at `0x0340`. The standard audio interface has its own isochronous endpoint
`0x83`, but it is not a transport-state notification channel.

### JPEG scale factor

`CustomPropScaleFactor` stores the selected value as both the requested and
current scale factor. On the DCR-HC24's simple hardware path,
`DevmanWriteScalingFactors` writes one byte directly to index `0x007c`.

The compiled fallback is 36 (`0x24`); the installed configuration used 20,
with a permitted range of 4 through 128. The initialization trace first
writes reset value `0x10` and later writes the configured `0x14`. It then
adapts downward through `0x13 … 0x09`. With the Sony quality wizard
configured to 80, four observed initialization sessions each write `0x10`
then `0x50`; later adaptive values range from `0x4e` through `0x5a`. This
confirms both that larger scale factors mean lower image quality and that
Sony's driver adjusts the register while capturing.

The 64-byte `0x0080` and `0x00c0` matrix payloads are byte-for-byte identical
between the normal- and low-quality traces. On this hardware, quality
selection is therefore independent of the fixed JPEG quality-50 tables used
to reconstruct standard JPEG files.

Reproduce the control comparison with:

```sh
./tools/extract_sony_control.py \
  captures/usb-record-mode-low-quality-piix3.pcapng \
  /tmp/lowq-control.jsonl --device-address 16
./tools/compare_sony_quality.py \
  /tmp/ohci-control.jsonl /tmp/lowq-control.jsonl
```

Another driver hardware path uploads two 64-byte scaling tables at `0x0080`
and `0x00c0`. It is mutually exclusive with the DCR-HC24 path that uploads
quantization matrices at those indices.

The initialization plan's TSV fields are:

```text
delay_us
bmRequestType
bRequest
wValue
wIndex
wLength
data_hex
source_capture_frame
```

The historical `handycam-capture` prototype validated every field, allowed
only request `0x88` and interface-0 `SET_INTERFACE`, preserved recorded
delays, and required the actual returned length to match `wLength`. The Rust
driver now performs the equivalent validation and replay.

The plan was extracted from a successful Windows/OHCI trace with:

```sh
./tools/extract_sony_init.py \
  captures/usb-record-mode-ohci.pcapng \
  ../crates/handycam-core/assets/record-mode-init.tsv \
  --device-address 16 \
  --start-frame 4196 \
  --end-frame 4643
```

The extractor's `SET_INTERFACE` frame map is specific to that source trace
because the Wireshark field renderer did not expose `wValue` for those five
Linux-usbmon setup packets. The alternate values were verified directly from
their raw eight-byte USB setup data.

## Stream startup

A verified userspace startup sequence is:

1. Open `054c:00c0`.
2. Confirm that interface 0 has no kernel driver.
3. Claim interface 0.
4. Replay `../crates/handycam-core/assets/record-mode-init.tsv` with the Rust driver.
5. Allocate multiple asynchronous isochronous transfers for both `0x81` and
   `0x82`.
6. Submit the two endpoint pipelines continuously.
7. Process every ISO packet independently, including zero-length packets.
8. On shutdown, cancel and reap every outstanding transfer.
9. Select interface 0 alt 0.
10. Release interface 0.

The reference implementation uses 32 one-packet transfers per endpoint. One
packet per transfer minimizes ambiguity in cross-endpoint completion order.
libusb does not expose usbfs's scheduled `start_frame`, so the live metadata
records callback order. A simultaneous usbmon trace can recover exact USB
frame numbers when required.

Testing showed:

- alt 5 without initialization: only zero-length packets;
- alt 5 plus active standard USB audio: only zero-length video packets; and
- the traced initialization followed by alt 5: immediate video data.

The Sony request sequence, rather than audio activation alone, is therefore
the necessary stream enable operation.

## Frame synchronization

Sony splits synchronization and compressed data across two endpoints.

### Boundary indication on `0x81`

The Windows driver treats an active `0x81` packet as a frame-boundary
indication when:

```c
(packet[0] & 0x08) != 0
```

Other bits and bytes in the five-byte packet remain undocumented.

### Record header on `0x82`

A new compressed record begins only when the following eight-byte structure
occurs at the start of an individual `0x82` ISO packet:

```text
offset  size  meaning
0       6     ff ff ff ff ff ff
6       1     flags plus timestamp bits 10..8
7       1     timestamp bits 7..0
```

The timestamp is:

```c
timestamp_ms = ((header[6] & 0x07) << 8) | header[7];
```

It is an 11-bit millisecond counter and wraps modulo 2048. At 25 fps,
successive frames normally advance by 40.

The upper five bits of byte 6 have not been assigned meanings.

The Sony driver sets a pending boundary flag from `0x81`, then consumes it
when it sees the six-`0xff` `0x82` header. A userspace implementation should
preserve that relationship:

```text
on 0x81 packet with byte-0 bit 3:
    boundary_pending = true

on descriptor-start 0x82 header:
    if boundary_pending:
        emit/start frame
        boundary_pending = false
```

The six-`0xff` sequence must be tested at the start of the ISO packet, not at
an arbitrary position in the concatenated endpoint stream.

In the direct Linux run, all 125 descriptor-start headers had a preceding
boundary. The observed boundary-to-header completion delay was approximately:

```text
minimum: 1.385 ms
median:  1.508 ms
p95:     1.574 ms
maximum: 1.634 ms
```

The corresponding USB-frame distance was normally one or two frames.

### Timestamp health

The Sony timestamp is useful when it advances normally, but it must not be
the only production presentation clock. In the full Windows/OHCI trace, 516
record headers included 310 normal 40-ms advances and 172 zero advances. The
longest run repeated timestamp 383 for 149 successive intervals over about
6.96 seconds.

All 515 bounded records in that trace still reconstructed and decoded, and
the repeated-timestamp tail contained distinct JPEG and decoded-pixel hashes.
Thus the endpoint continued delivering new compressed records while its
timestamp was stuck. This is a likely explanation for the Sony AVI appearing
frozen: a timestamp-sensitive Windows delivery path can hold its last frame
even though USB payload continues.

A Linux implementation should use scheduled USB-frame numbers when its
transport exposes them, otherwise monotonic packet arrival/callback time, to
maintain the 25-fps presentation clock. The Sony timestamp remains valuable
as a health and discontinuity signal.

This policy has been tested offline against the full OHCI extraction:

```sh
./tools/build_sony_av.py \
  captures/extracted-record-mode-ohci-all \
  /tmp/sony-av
ffprobe /tmp/sony-av/capture.mkv
```

The longest stored session produced 192 distinct decodable MJPEG frames over
9.040 seconds and 144,640 matching stereo sample frames. Their first packets
mapped to the same unwrapped one-millisecond USB frame. Individual video PTS
values come from each record's USB frame, preserving missing/delayed-frame
gaps rather than falsely squeezing the data into 7.680 seconds at 25 fps.
This remains synchronized despite 155 zero Sony-timestamp deltas.

## Compressed record format

A record extends from one descriptor-start `0x82` header to the next:

```text
six-ff marker | timestamp/flags | unstuffed baseline-JPEG entropy | padding
```

The final record in a stopped capture is not known to be complete because it
has no following header. The first record after a long period with alt 5
active but no submitted ISO reads may also span a startup discontinuity.

`split_sony_records.py` uses packet metadata rather than raw byte scanning to
find headers and writes one `.bin` file per record.

## JPEG reconstruction

The bytes after the eight-byte Sony header are raw baseline-JPEG entropy, not
a complete JPEG file:

- there is no SOI marker;
- there are no DQT, SOF, DHT, or SOS segments;
- entropy byte `0xff` is not followed by the JPEG-required stuffing byte
  `0x00`; and
- zero bytes may pad the end of the record.

The decoded format is:

```text
dimensions:    320x240
precision:     8 bit
components:    Y, Cb, Cr
sampling:      4:2:0 (Y 2x2, Cb 1x1, Cr 1x1)
quantization:  standard JPEG quality-50 luma/chroma tables
Huffman:       standard baseline JPEG DC/AC tables
```

The 64-byte tables written to Sony indices `0x0080` and `0x00c0` are the
standard quality-50 matrices transposed for the camera's register layout.

To turn one Sony record into a JPEG:

1. Remove the eight-byte Sony header.
2. Remove trailing zero alignment padding.
3. Insert `0x00` after every entropy byte `0xff`.
4. Prefix a conventional baseline JPEG header containing:
   - SOI;
   - quality-50 DQT tables;
   - a 320x240 4:2:0 SOF0;
   - standard DC/AC DHT tables; and
   - a matching SOS.
5. Append EOI.

`decode_sony_records.py` generates a canonical header with Pillow and then
performs those transformations. All 124 records bounded by a following
header in the first direct Linux run decoded successfully. FFmpeg accepted
the complete output sequence without errors.

## Verified direct-Linux run

The first five-second direct run reported:

| Endpoint | Packets | Nonempty | Bytes | Packet errors |
|---|---:|---:|---:|---:|
| `0x81` | 5,000 | 127 | 635 | 0 |
| `0x82` | 5,001 | 1,328 | 957,800 | 0 |

Framing:

```text
boundary packets:       127
descriptor-start heads: 125
candidate samples:      125
header cadence:         25 in each of five seconds
```

After discarding the initialization-discontinuity record, 123 complete
records remained and every timestamp advanced by 40 ms. They were muxed as:

```text
MJPEG, 320x240, 25 fps, 123 frames, 4.92 seconds
```

Primary evidence:

```text
captures/usb-linux-libusb-init-first.pcapng
SHA-256 7f65eb879a816bdcc2dba5903d14a11d7589e8bff748fdb4b12719b567751688

captures/live-linux-init-first/ep82.bin
SHA-256 ebcca44e15c7982fb5df1684c1d098c1b87d41b60a6d76ca4d5803b25d6a43d5

captures/usb-linux-direct-video-first.avi
SHA-256 696ca51715b53eee47df36ab6ae290bafa507ab3b29ebe5371795cefa63a7db1
```

## Reference workflow

Build and capture:

```sh
make

../target/release/handycam stream \
  --duration 5 \
  --init ../crates/handycam-core/assets/record-mode-init.tsv \
  ../reverse-engineering/captures/live-linux
```

Split and decode:

```sh
./tools/split_sony_records.py \
  captures/live-linux \
  captures/records-linux

./tools/decode_sony_records.py \
  captures/records-linux \
  captures/frames-linux
```

Create an MJPEG AVI after excluding a discontinuous first frame:

```sh
ffmpeg \
  -framerate 25 \
  -start_number 2 \
  -i captures/frames-linux/frame-%04d.jpg \
  -c:v copy \
  captures/video.avi
```

## Remaining unknowns

The following work would improve the protocol from a working replay into a
fully semantic implementation:

- minimize the 223-operation initialization plan;
- assign meanings to the one-byte Sony register indices;
- decode the fields in the `0x0300` and `0x0340` blocks;
- identify the upper five flag bits in record-header byte 6;
- determine whether alternate settings map to explicit quality or bandwidth
  modes;
- bound monotonic A/V drift with a 30-minute live capture;
- decode playback bytes 6–7 against a displayed playhead/timecode;
- search playback DV payload/subcode and control requests for clip capture
  date/time metadata;
- identify status byte 9;
- identify the secondary `0x23`, `0x30`, and `0x31` shuttle/state commands;
- test other cameras sharing USB ID `054c:00c0`; and
- fault-test capture reconnects and longer streams.

None of those unknowns prevents record-mode video capture, synchronized
Matroska recording, or the five primary playback transport operations.
