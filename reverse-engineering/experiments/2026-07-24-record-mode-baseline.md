# Record-mode USB capture baseline

Date: 2026-07-24

Camera: Sony DCR-HC24, USB ID `054c:00c0`

Guest: Windows XP, libvirt domain `winxp`

Mode: camera/record mode. Tape transport controls are not applicable to this
experiment.

## Windows device state

The camera is a three-interface USB composite device:

- interface 0: Sony vendor-specific video/control, driven by
  `sonypvs1.sys` through `sonypvs2.inf`;
- interface 1: standard USB Audio Control;
- interface 2: standard USB Audio Streaming, stereo 16-bit PCM at 16 kHz.

Windows Device Manager/WMI reports `Sony Digital Imaging Video` with status
`OK`. Sony Video Capturing Tool selects `USB Streaming` and enables
`Start capturing` after a fresh virtual detach/attach PnP cycle.

Starting capture creates:

```text
C:\Program Files\Sony Corporation\Picture Package\CapturingTool\'26_07_24_00\TEMP.AVI
```

The leading apostrophe in the directory name is literal. Pressing the large
`PAUSE` control ends the attempt with `Image capturing failed`, after which
the application deletes `TEMP.AVI`.

## USB descriptor map

Interface 0 has alternate settings 0 through 7:

- endpoint `0x81`, isochronous IN, 8 bytes per USB frame for alt 1-7;
- endpoint `0x82`, isochronous IN, from 256 bytes at alt 1 through 1023 bytes
  at alt 7.

Interface 2 alt 1 uses endpoint `0x83`, isochronous IN, 64 bytes per USB
frame for PCM audio.

The Sony driver briefly selects video alt 7, falls back to alt 5, and starts
audio interface 2 alt 1. Representative control-transfer sequence from the
baseline trace:

```text
relative time  interface  alternate
230.152641     0          7
230.219175     0          5
231.018754     0          0
231.226720     0          7
231.286615     0          5
232.070419     2          1
```

## Host trace

Host tracing used Linux `usbmon1` and direct `dumpcap`:

```sh
sudo modprobe usbmon
sudo setfacl -m u:alex:rw /dev/usbmon1
sudo setcap cap_net_raw,cap_net_admin=eip /usr/bin/dumpcap
/usr/bin/dumpcap -i usbmon1 -w ../captures/usb-record-mode-baseline.pcapng
```

Direct `dumpcap` works. Live capture through the `tshark` wrapper still
reported `EACCES`, so `tshark` is used only for offline analysis.

Baseline artifact:

```text
../captures/usb-record-mode-baseline.pcapng
47,281 packets, 0 dropped, 523.311271 seconds, 91,019,528 bytes
SHA-256 0dae468dff096fb68d399024b74c20534b4e6ba97980f03afe0ae13331a1155c
```

For the reconnected camera at USB address 10, the host observed:

```text
endpoint  successful completions  cancelled completions  ISO error_count
0x81      2,115                   4                      0
0x82      5,972                   8                      0
0x83      1,566                   2                      0
```

The cancelled completions have status `-2` and occur at stream transitions.
All ordinary completions have URB status 0 and ISO error count 0.

Endpoint `0x82` contains real structured video data. Concatenating its
successful payloads produced 17,602,856 bytes and 5,902 apparent records
beginning with six `0xff` bytes. Record spans are commonly about 2.3-3.25
KiB. This is strong evidence that the physical camera and host USB link are
delivering video.

## Preserved incomplete AVI

A live copy was taken before pressing `PAUSE`:

```text
../captures/usb-record-mode-traced-live-temp.avi
738,304 bytes
SHA-256 c00a8acffe099030452fdfe33ac015b939dccda3b4eedf98704111473039c37a
```

It is not a finalized AVI:

- bytes 0 through 130,047 are zero;
- no `RIFF`, `AVI `, `LIST`, `movi`, `00db`, or `00dc` markers exist;
- `01wb` audio chunks begin at offset 130,048 and repeat every 22,528 bytes;
- matching `JUNK` chunks are present;
- `ffprobe` reports invalid data.

This shows that the standard USB audio path works while the Sony video driver
does not emit video frames to the capture application.

## QEMU buffer A/B test

QEMU's `usb-host` device was tested with a creation-time override:

```xml
<qemu:property name='isobufs' type='unsigned' value='16'/>
<qemu:property name='isobsize' type='unsigned' value='64'/>
```

The live values were verified with QMP. The tuned run still ended with
`Image capturing failed`.

Tuned artifacts:

```text
../captures/usb-record-mode-tuned-16x64.pcapng
23,411 packets, 0 dropped, 297.084837 seconds, 43,869,336 bytes
SHA-256 aec2278c32ecf536d0c6e9519b52699e663d23c91e563677622bc22b419202b8

../captures/usb-record-mode-tuned-16x64-live-temp.avi
513,024 bytes
SHA-256 c3970da24e0b9a03d483654dfd9a8e18b289d0cb474e55faf1398d33a0d15159
```

At USB address 11, the tuned trace again showed status 0 and ISO error count
0 for all ordinary `0x81`, `0x82`, and `0x83` completions. The tuned AVI has
the same zero-header/audio-only layout as the baseline.

The tuning was therefore removed from the persistent libvirt definition.
The following restart cleared the live tuning values.

### One-frame host batching test

The first attempted `isobsize=1` run was invalid: a later live
detach/attach replaced the tuned `ua-camera` device with a transient
`hostdev0` using QEMU's default `isobufs=4`, `isobsize=32`. Its artifacts are
retained for provenance but must not be used as evidence for the one-frame
test:

```text
../captures/usb-record-mode-isobsize1.pcapng
SHA-256 fb11e38a661d53f5a695ed154800771333f8fa28d7888d85cc59b99669957cc4
```

The corrected run used a full restart, no hotplug, and QMP verification that
`ua-camera` had `isobufs=64`, `isobsize=1`. Capture still failed and produced
another audio-only AVI:

```text
../captures/usb-record-mode-isobsize1-real.pcapng
275,185 packets, 0 dropped, 127.294902 seconds, 40,401,356 bytes
SHA-256 92f37cfade5d86772a5b02a7d0d5884299f35bd3d7a11b8c406b01968aa9317f

../captures/usb-record-mode-isobsize1-real-live-temp.avi
805,888 bytes
SHA-256 d18910752132e8a05a931c545697f44fb13178a81db515a609da956d678c276d
```

The AVI has 130,048 leading zero bytes, no RIFF/video markers, and 30 each of
`01wb` and `JUNK`. Boundary-preserving extraction gave:

| Endpoint | URBs | ISO descriptors | Nonempty | Bytes |
|----------|-----:|----------------:|---------:|------:|
| `0x81` | 30,138 | 30,138 | 1,116 | 5,580 |
| `0x82` | 80,173 | 80,173 | 11,665 | 7,993,132 |

Every URB has exactly one ISO descriptor and all descriptor statuses and ISO
error counts are zero. Host libusb transfer batching is therefore not the root
cause. The persistent tuning override was removed afterward.

## USB controller A/B test

The default VM exposes ICH9 EHCI plus three companion ICH9 UHCI controllers.
XP reports them as Intel devices `293A`, `2934`, `2935`, and `2936`. The
full-speed camera normally attaches to ICH9 UHCI.

For comparison, a separate QEMU PIIX3 UHCI controller was added and only
`ua-camera` was moved to USB bus 1. XP installed it as:

```text
Intel(R) 82371SB PCI to USB Universal Host Controller
PCI\VEN_8086&DEV_7020...
```

The new USB topology created a new Sony device instance, for which the
existing `sonypvs2.inf` driver was installed successfully. Capture still
ended with `Image capturing failed`.

```text
../captures/usb-record-mode-piix3.pcapng
64,465 packets, 0 dropped, 521.358842 seconds, 148,479,508 bytes
SHA-256 21fffa7599e0bca27aa8785224a193474d0ae98568e1a566065fce5697edcd3a

../captures/usb-record-mode-piix3-live-temp.avi
670,720 bytes
SHA-256 f7e71ba5f7b00856a59ea9a3bc273d307478ca93732d76ddec6a7386e0446590
```

Endpoint `0x82` had 9,807 successful completions, zero ordinary URB errors,
and zero ISO errors. The AVI again begins with 130,048 zero bytes and contains
only `01wb` audio plus `JUNK`, with no video chunks.

PIIX3 was a negative result, but it did not exhaust materially different USB
1.x host-controller implementations; OHCI was tested later.

## Independent DirectShow application test

The VM originally exposed ICH6 HDA, for which XP had no driver. Windows Movie
Maker therefore refused to start with `required audio hardware cannot be
found`. The persistent sound device was changed to AC'97 and XP installed its
inbox driver:

```text
Intel(r) 82801AA AC'97 Audio Controller
device status: OK
```

Movie Maker then launched and enumerated `Sony Digital Imaging Video2` plus
`USB Audio Device`. It negotiated a best-quality 320x240, 30 fps WMV profile.
On the default ICH9 USB controller, however, a 20-second attempt remained at
duration `0:00:00` and created only a 2,754-byte `HandycamTest.wmv`.

This independent application reproduced the absence of video samples. The
failure was therefore below Sony Capturing Tool, inside the Sony kernel
driver/DirectShow source path or its interaction with the guest USB HCD.

## Sony quality calibration

Sony's bundled `USBStrTool.exe` is a three-step USB Streaming Tool:

1. select the USB audio device;
2. reduce video quality if frame dropping is noticeable;
3. adjust brightness.

The original driver registry values were:

```text
IsoBandwidth = 1023
SF           = 20
SFMin        = 4
SFMax        = 128
```

Completing the wizard at a lower-quality position auto-calibrated `SF=80` and
`SFMin=79`. A new capture still failed:

```text
../captures/usb-record-mode-low-quality-piix3.pcapng
31,779 packets, 0 dropped, 399.761469 seconds, 60,403,520 bytes
SHA-256 46e92ec194d2799d56877a143b898fbaeb651869c838a00b52048868936d03d8

../captures/usb-record-mode-low-quality-piix3-live-temp.avi
535,552 bytes
SHA-256 6b995d2a1b39120b9eb67bcf8ecc527a54d7d98f44626e719c6d1a1a7f4eeb46
```

The quality setting clearly affects the compressed stream: median
six-`0xff` record span fell from about 3,152 bytes to 1,824 bytes. Endpoint
completions remained clean, but the AVI was still audio-only. This rules out
simple excessive compressed frame size/quality as the cause. The original
`SF=20`, `SFMin=4` values were restored afterward.

## Driver artifacts

The installed driver and INF were preserved under the ignored
`../artifacts/sony-driver/` directory:

```text
sonypvs1.sys
SHA-256 81bcba6de9cf540c66b4226bd5d46084295f41822bc1a7eb938277737f46cc76

sonypvs2.inf
SHA-256 d78ba767a343b210c0bd1d33ceb9d98378fe289a750761b15cb9664febd6b219

USBStrTool.exe
SHA-256 1125ffe6f6f9048cd45977348b378bae0363a5fd94b26dad11e80ae3067887f6
```

`sonypvs1.sys` contains `USBISO` diagnostics, raw pixel format names
`I420`, `IYUV`, `YUY2`, `Y41P`, and `Y411`, and JPEG Huffman-table material.
This supports the hypothesis that the driver decodes the structured `0x82`
payload into raw DirectShow video.

The driver also contains appended CodeView NB11 debug data. The
`sstGlobalPub` / `S_PUB32` table yielded 320 public symbols with
`extract_codeview_symbols.py`. Relevant RVAs include:

| RVA | Symbol |
|----:|--------|
| `0x14244` | `transfer_completion` |
| `0x15764` | `JpegImageDataSet` |
| `0x157c4` | `JpegDecordPicture` |
| `0x16d42` | `JpegDecompress` |
| `0x19222` | `StreamCompleteRead` |
| `0x195ce` | `StreamDropControl` |
| `0x196aa` | `StreamSyncCheck` |
| `0x1973a` | `StreamErrorRecovery` |

Disassembly using those names established:

- `transfer_completion` derives a frame-boundary-ready flag from endpoint
  `0x81` byte 0 bit 3;
- only a subsequent endpoint `0x82` record beginning with six `0xff` bytes
  queues `StreamCompleteRead`;
- `JpegImageDataSet` checks the first four bytes for `0xff`, begins decoder
  input at byte 8, and clears byte 0;
- the record layout is
  `ff ff ff ff ff ff | timestamp/flags (2 bytes) | Sony JPEG entropy data`;
- `StreamCompleteRead` treats bytes 6/7 as an 11-bit millisecond timestamp,
  converts it to 100 ns units, and wraps it at two seconds.

No active calls to the retained `DbgPrint`/`USBISO` strings were found in the
release driver.

## Boundary-preserving extraction

`extract_sony_stream.py` was validated against the baseline pcapng. It uses
`usb.iso.iso_len`, not `usb.iso.iso_actual_len`: in Linux usbmon completion
records the former contains all 32 per-frame lengths while the latter is
unset. Nonempty `usb.iso.data` occurrences map in order to nonzero descriptor
lengths.

The extractor reproduced these successful-completion totals while retaining
all zero-length frame slots:

| Endpoint | URBs | ISO descriptors | Nonempty | Bytes |
|----------|-----:|----------------:|---------:|------:|
| `0x81` | 2,115 | 67,680 | 2,001 | 10,005 |
| `0x82` | 5,972 | 191,104 | 26,950 | 17,602,856 |

All descriptor statuses were zero. Every decoded payload length matched its
declared descriptor length and every URB's descriptor lengths summed to its
reported length.

Baseline extractor hashes:

```text
iso-packets.jsonl
SHA-256 55925ea81c4d5a16973823bad9d066870a2f8c6231434e238ef893d1172fffc0

ep81.bin
SHA-256 d3f144603d33c1c2269a3bb23472337a30bf68ef9c728c378aacb119f44e63bc

ep82.bin
SHA-256 9b01b500d3a135c48cb9e64b54444c03aa52624fc9a95debcef8e049a2c4448a
```

## OHCI success

A separate QEMU `pci-ohci` controller was added and only the camera was moved
to USB bus 1 port 1. XP installed its inbox driver:

```text
Standard OpenHCD USB Host Controller
PCI\VEN_106B&DEV_003F...
```

The Sony interfaces remained `OK`. Sony Capturing Tool then showed a live
camera preview, and pausing produced `Image capturing halted — Save` rather
than `Image capturing failed`. Saving finalized:

```text
../captures/usb-record-mode-ohci-final.avi
29,468,160 bytes
SHA-256 daca50126e66e0a894b1dc82640d97aabd4d943cb489d596d7a4bdc5311c130e
```

`ffprobe` reports:

```text
format: AVI
duration: 7.240000 seconds
bit rate: 32,561,502 bit/s
video: rawvideo, BGR24, 320x240, 25 fps
audio: PCM signed 16-bit little-endian, 44.1 kHz, stereo
```

Counting actual video packets reveals substantial loss in this first
successful file. The AVI header declares 181 video slots, while `ffprobe`
reads 79 230,400-byte frames. Across presentation timestamps 6 through 186,
102 slots are missing in 42 gaps; the largest gap is 20 frames. OHCI therefore
fixes video delivery and AVI finalization, but this run does not yet
demonstrate continuous 25 fps capture.

A frame extracted with ffmpeg clearly shows the live camera scene. A copy
taken before finalization is also preserved:

```text
../captures/usb-record-mode-ohci-live-temp.avi
26,122,752 bytes
SHA-256 b93778ab3c00d2217cb9b6c02a0cd36641a58cff5a33d9496e1ae3df9e0a9efe
```

The corresponding successful host trace is:

```text
../captures/usb-record-mode-ohci.pcapng
28,767 packets, 0 dropped, 181.033296 seconds, 28,987,148 bytes
captured packet data: 28,064,310 bytes
SHA-256 f4642d1197e85b962076bb0af99bf100aa0d33bc2071fbd3e848ddfcfec5c02d
```

The trace begins after enumeration, so the known camera address 16 was passed
explicitly to the boundary-preserving extractor:

```sh
../tools/extract_sony_stream.py --device-address 16 \
  ../captures/usb-record-mode-ohci.pcapng \
  ../captures/extracted-record-mode-ohci
```

The known-good extraction contains:

| Endpoint | URBs | ISO descriptors | Nonempty | Bytes |
|----------|-----:|----------------:|---------:|------:|
| `0x81` | 427 | 13,664 | 376 | 1,880 |
| `0x82` | 579 | 18,528 | 3,349 | 2,327,864 |

All URB and descriptor statuses are zero. Its hashes are:

```text
iso-packets.jsonl
SHA-256 00b60193b1424d7772147475ef990ae663e9e9f41752214194793acb7f2f25d5

ep81.bin
SHA-256 6f8020500d3f8d8c119dabbd4f6d4eb246f21ba523cc85b15b548e3529c7393c

ep82.bin
SHA-256 610c033f06f931610b8c06b11f3fafb1bb999006622696ef13c7ee769112b95c
```

### Frame-delivery and apparent-freeze analysis

The saved AVI appears to show motion for roughly six seconds and then a held
frame. This is not a sequence of duplicate frames in the file. `framemd5`
shows that all 79 stored raw frames have distinct pixel hashes:

```text
second       0  1  2  3  4  5  6  7
AVI frames  11 14 11 17 10 11  3  2
```

There are 74 frames before six seconds and only five afterward, at AVI
timestamps 6.64, 6.84, 6.96, 7.12, and 7.44 seconds. A player holds the most
recent frame across the missing slots, creating the apparent freeze.

Host artifact timestamps identify the first OHCI video interval as the actual
capture: it ran from 15:26:30.405 to 15:26:37.900, and the live `TEMP.AVI` copy
was made later at 15:27:17.470. The matching USB interval remained active for
7.495 seconds:

```text
second                    0  1  2  3  4  5  6  7
0x81 boundary packets    27 25 24 23 25 25 25 10
0x82 start headers       26 25 25 23 24 25 23 10
ordering candidates      26 21 20 16 19 19 17  7
```

It contains 184 endpoint-`0x81` boundary packets and 181 endpoint-`0x82`
descriptor-start headers. Of 180 consecutive header transitions, 153 advance
by the expected 40 ms, ten repeat a timestamp, and none repeat the complete
compressed payload. Fresh compressed camera data therefore continues through
the interval, including after six seconds.

Disassembly confirms that `transfer_completion` tests bytes 0–5 at the start
of each `0x82` payload. If they are all `0xff` and the `0x81` boundary flag is
set, the routine may queue `StreamCompleteRead`; it still has additional gates
for pending asynchronous work, output-buffer state, error/drop state, and rate
control. A host-completion-order approximation gives:

| Run | `0x81` boundaries | `0x82` start headers | Candidate samples | Boundary survival |
|-----|------------------:|---------------------:|------------------:|------------------:|
| OHCI | 376 | 516 | 294 | 78.2% |
| ICH9 baseline | 2,001 | 4,899 | 260 | 13.0% |
| PIIX3 | 3,960 | 8,014 | 422 | 10.7% |
| ICH9 `isobsize=1` | 1,116 | 2,063 | 231 | 20.7% |

This supports cross-endpoint ordering as the reason OHCI works at all, but the
model predicts 145 candidates during the captured interval while only 79
frames reach the AVI. In particular it predicts 17 and seven candidates in
the final two partial seconds, versus three and two stored frames. The
six-second collapse is therefore not explained by stopped USB delivery or by
the simple boundary/header flag alone. The next target is the driver's
pending-decode and output/error gates before it queues `StreamCompleteRead`.

One of those gates has a direct registry control. `CPULoad` is loaded into
device-extension offset `+0x67c`; `transfer_completion` computes:

```text
threshold = CPULoad * frames_seen_while_decode_busy / 128
```

It queues a new sample only when the available-frame counter exceeds that
threshold (or exceeds 100). The compiled fallback at `0x181f6` is
`CPULoad=128`, while Sony's INF contains a commented optional
`CPULoad=0x40` for every hardware model. In the common case where one frame
arrives while the prior asynchronous decode is pending, 128 makes the next
available frame fail `1 > 1`, causing one additional drop; 64 makes the
threshold truncate to zero, allowing it through.

The setting is read from the selected model path:

```text
HKLM\SOFTWARE\SONY PVC\Sony Digital Imaging\Rel1
HKLM\SOFTWARE\SONY PVC\Sony Digital Imaging\Rel2
HKLM\SOFTWARE\SONY PVC\Sony Digital Imaging\Rel3
```

An OHCI capture with a reversible `CPULoad=64` override is therefore the next
focused A/B test. The driver reads the registry only during hardware
initialization, so the camera device or VM must be restarted after changing
it.

The corrected registry query found `CPULoad=0x80` in Rel1, Rel2, and Rel3.
All three were changed to `0x40` and verified. At that point the camera was
absent and `sc query sonypvs1` reported the kernel driver `STOPPED`, so the
next camera connection loaded the override without another VM restart.

That test did not reach capture. XP bugchecked while Capturing Tool enumerated
its source after a hidden instance had been killed and the application
relaunched. The USB trace was stopped and preserved with 27,032 packets and
zero dumpcap drops. XP produced bugcheck `0x1000008e` with access-violation
parameter `0xc0000005`.

A second recovered dump records an earlier `0x100000c5` crash at 16:05,
before `CPULoad` was changed. Both dumps name `CapturingTool.exe` as the
active process and contain the Sony and USB drivers in their module lists.
The earlier crash makes a capture-tool lifecycle or driver-handle cleanup race
a credible common trigger; the `CPULoad=64` result is therefore inconclusive,
although the override could still alter timing. Exact hashes and System log
parameters are recorded in
[`../artifacts/crashes/2026-07-24`](../artifacts/crashes/2026-07-24/README.md).

After the second restart, `./sony-cpuload set-128` restored and verified
`CPULoad=0x80` in all three model keys. Further capture work should first
establish a clean, single-process launch at the original value.

That clean reproduction was performed after a full XP reboot. There was
exactly one console-session Capturing Tool process, live preview worked, and
capture started. Pausing after approximately 15 seconds entered
`Progress 0/35`, then XP bugchecked with `0x100000d1`. The faulting address
maps through the dump's loaded-module table to `USBPORT.SYS+0x9d43`; the dump
records `System` as the current process. The trace contains 10,659 packets
with zero dumpcap drops.

The surviving 6,904,832-byte `TEMP.AVI` contains only seven BGR24 video frames
(0.28 seconds) plus PCM audio. This rules out multiple Capturing Tool instances
and `CPULoad=64` as necessary causes of the instability and reproduces severe
video sample-delivery loss at the original setting. Full hashes and crash
parameters are in
[`../artifacts/crashes/2026-07-24`](../artifacts/crashes/2026-07-24/README.md).

The VM has now served its essential reverse-engineering purpose. Further
symbol-aware XP dump analysis may distinguish a Sony request-lifetime bug from
an XP USBPORT/QEMU OHCI problem, but it is not a prerequisite for a Linux
userspace transport implementation.

## Direct Linux implementation

The camera was returned to the host after shutting down XP. Linux enumerated
`054c:00c0` at full speed with no kernel driver on vendor interface 0 and
`snd-usb-audio` on interfaces 1 and 2. The installed libusb 1.0.29 development
files were sufficient to build `handycam-capture`.

Two negative probes established the startup boundary:

- selecting interface 0 alt 5 and scheduling one-packet asynchronous ISO
  transfers on `0x81` and `0x82` returned 5,000 clean zero-length packets per
  endpoint;
- activating audio interface 2 alt 1 through ALSA at the same time still
  returned only zero-length video packets.

`extract_sony_init.py` then recovered the control block preceding the first
known-good Windows stream. `../../crates/handycam-core/assets/record-mode-init.tsv` retains 223 Sony
request-`0x88` reads/writes and interface-0 alternate changes, with exact
payloads and inter-request timing. Replaying it before submitting transfers
enabled the camera stream directly on Linux.

The five-second direct run produced:

```text
0x81: 5,000 packets, 127 nonempty, 635 bytes, zero packet errors
0x82: 5,001 packets, 1,328 nonempty, 957,800 bytes, zero packet errors
127 boundary packets
125 descriptor-start video headers
125 boundary/header candidate samples
25 headers in each of five seconds
```

The usbmon trace contains 20,582 packets with zero dumpcap drops:

```text
../captures/usb-linux-libusb-init-first.pcapng
SHA-256 7f65eb879a816bdcc2dba5903d14a11d7589e8bff748fdb4b12719b567751688
```

The endpoint files produced live and re-extracted from usbmon have identical
hashes. This validates the libusb logger against the existing offline
extractor.

The timestamp field is 11 bits and therefore wraps modulo 2048 ms, correcting
the earlier modulo-2000 analysis. Apart from the first initialization
discontinuity, all 123 bounded transitions advance by exactly 40 ms.

`split_sony_records.py` recovered 125 Sony records. The payload after each
eight-byte Sony header is an unstuffed baseline-JPEG entropy stream. The
traced quantization matrices are transposed standard JPEG quality-50 tables;
the stream uses standard Huffman tables and 4:2:0 sampling.
`decode_sony_records.py` supplies a conventional 320x240 JPEG header, inserts
`00` after entropy `ff` bytes, appends EOI, and successfully decodes all 124
records bounded by a following header. FFmpeg accepted the whole sequence
without decode errors.

A cadence-clean preview discarding the startup record is:

```text
../captures/usb-linux-direct-video-first.avi
MJPEG, 320x240, 25 fps, 123 frames, 4.92 seconds
SHA-256 696ca51715b53eee47df36ab6ae290bafa507ab3b29ebe5371795cefa63a7db1
```

The comparison is reproducible with:

```sh
../tools/analyze_sony_framing.py \
  ../captures/extracted-record-mode-ohci-all \
  ../captures/extracted-record-mode-baseline \
  ../captures/extracted-record-mode-piix3 \
  ../captures/extracted-record-mode-isobsize1-real
```

The OHCI camera-only controller and AC'97 sound device are retained in the
persistent libvirt definition. Normal QEMU `isobufs=4`, `isobsize=32` camera
batching is active.

## Interpretation and next experiments

The successful OHCI capture proves that the camera stream, Sony decoder, and
capture applications can all work in this VM. The earlier failure is specific
to guest-HCD behavior on the tested UHCI paths. It is not explained by a dead
stream, insufficient bandwidth, host-side URB errors, compressed frame size,
or host libusb completion batching.

The main reverse-engineering question is now which OHCI-versus-UHCI
guest-visible difference preserves the endpoint `0x81` boundary indication
and following `0x82` record relationship expected by `sonypvs1.sys`.

Useful next work is:

- make a longer OHCI capture and quantify frame continuity and real-time rate;
- run the boundary-preserving extractor on the OHCI trace and compare
  `0x81`/`0x82` completion grouping, USB frame numbers, and ordering with the
  failed UHCI traces;
- reconstruct the six-`0xff` Sony JPEG records and compare their decoded
  output with frames produced by the Windows driver;
- prototype the USB protocol in Linux userspace before committing to a V4L2
  kernel driver;
- preserve this OHCI VM configuration as the working behavioral oracle.
