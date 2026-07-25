# From a blue-screening Windows XP VM to a Linux Handycam capture tool

An old Sony Handycam is a small time capsule. The tape mechanism belongs to
one era, its USB connection to another, and the software CD in the box to a
third. The Sony DCR-HC24 in this project still worked perfectly well as a
camera. The problem was persuading a modern Linux machine to receive the
video it was already willing to send.

This is the story of how the problem moved from “make Sony's Windows XP
software capture an AVI” to “receive and decode the camera directly with
libusb”—and why several apparent failures turned out to be useful protocol
documentation.

## Starting with the historical software

The conservative first step was to recreate the camera's original habitat.
We built a Windows XP guest under QEMU/libvirt, attached the Sony software
ISO, passed USB device `054c:00c0` into the VM, and installed Picture Package.

There was an immediate hint that the connection was not completely broken:
Sony's application could communicate with the camera. In playback mode it
could operate the transport controls. In record mode it could recognize “USB
Streaming.” Something was working below the application, but video capture
was not.

To make the VM usable as a laboratory rather than a desktop we had to operate
manually, we added two control paths:

- Cygwin OpenSSH for process, file, registry, and service access; and
- libvirt/QEMU monitor input plus screenshots for the interactive XP UI.

VNC remained available as a fallback, but paced QMP keyboard and absolute
pointer events were more reliable for automation. Even that required a small
lesson: sending an entire string or a move-and-click batch at once could
overflow XP's input handling or click at the previous pointer location.
Individual paced events fixed it.

This setup work was not glamorous, but it mattered. Reverse engineering is
much easier when every test can be repeated, observed, and recorded.

## The first important evidence was a broken AVI

Sony's capture application created `TEMP.AVI`, but the file did not contain
video. It had large zero-filled regions and repeating audio chunks. The
standard USB audio side of the camera was clearly working; the proprietary
video side was not reaching the application as completed frames.

That distinction was valuable. “Capture failed” could have meant:

- the camera had stopped sending;
- USB passthrough had corrupted transfers;
- Sony's kernel driver could not recognize frame boundaries;
- the driver received frames but failed to decode them; or
- the application mishandled valid driver output.

The incomplete AVI eliminated neither the camera nor USB, but it told us
where to look next.

## Watching USB directly

We enabled Linux `usbmon` on the host and captured the traffic around the XP
guest with `dumpcap`. The camera appeared as a three-interface composite
device:

- vendor-specific video/control on interface 0;
- standard USB Audio Control on interface 1; and
- standard stereo PCM streaming on interface 2.

Interface 0 exposed two isochronous IN endpoints. Endpoint `0x81` allowed
eight bytes per USB frame. Endpoint `0x82` scaled from 256 to 1023 bytes
depending on the selected alternate setting.

The failed traces contained substantial, clean data on `0x82`. Ordinary URBs
completed with status zero and no ISO descriptor errors. The data was
structured, not noise: thousands of apparent records began with six `0xff`
bytes.

That result changed the working hypothesis. The physical camera and host USB
link were delivering video. The failure was higher up.

## A detour through host-controller behavior

The XP guest initially used QEMU's emulated ICH9 EHCI/UHCI stack. We tried
larger QEMU isochronous buffers, one-frame host batching, lower Sony quality
settings, and a separate PIIX3 UHCI controller. The endpoint traffic remained
clean, but Sony still produced audio-only or invalid AVIs.

Then we moved only the camera to a separate QEMU `pci-ohci` controller.

For the first time, Sony's preview showed live video and the capture tool
finalized a real AVI. It contained raw 320x240 BGR video at 25 fps plus PCM
audio.

The controller result was more than a workaround. It suggested that Sony's
driver depended on completion ordering between its two isochronous endpoints.
UHCI and OHCI were presenting valid bytes but not equivalent timing or
grouping.

The first successful AVI still had a problem: it appeared to move for about
six seconds, then freeze. Frame analysis showed that the file did not contain
hundreds of duplicated images. It contained only 79 actual video frames
spread across 181 nominal frame slots. Players simply held the most recent
frame whenever Sony failed to deliver a new one.

Meanwhile, the matching USB interval continued to contain roughly 25 fresh
compressed headers every second. The camera never froze. The Windows driver
stopped completing enough decoded samples.

Later, a descriptor-level A/V pass made that result more specific. The final
roughly seven seconds contained 150 successive headers carrying the same
11-bit camera timestamp. Yet every bounded record still reconstructed as a
valid JPEG, and the decoded images were distinct. The video payload had not
stopped; its embedded clock had. That explains how Sony's timestamp-sensitive
delivery path could freeze while a Linux receiver that paces from USB frames
continues to display the records.

## Asking Sony's driver what it expected

`sonypvs1.sys` contained old CodeView NB11 debugging data. Recovering its
public symbol table gave us 320 names, including:

- `transfer_completion`;
- `StreamCompleteRead`;
- `JpegDecordPicture`;
- `JpegDecompress`;
- `decode_block`;
- `search_huffman`; and
- `DevmanWriteQuantizationMatrices`.

The disassembly confirmed the cross-endpoint framing rule:

1. endpoint `0x81`, byte 0, bit 3 sets a boundary-ready flag;
2. a later `0x82` packet beginning with six `0xff` bytes consumes that flag;
3. only then can the driver queue a completed video sample.

That explained why valid endpoint data could fail under one virtual host
controller and work under another.

The driver also exposed a rate-control registry value named `CPULoad`. Its
effective threshold was:

```text
CPULoad * frames_seen_while_decode_busy / 128
```

Sony's installed default was 128, while the INF contained an optional value
of 64. It was a plausible explanation for dropped output frames, so we tried
a reversible A/B test.

## When debugging the oracle became the problem

The XP VM began to blue-screen. We preserved three small kernel dumps:

- an earlier pool-corruption-style crash before changing `CPULoad`;
- an access violation during the `CPULoad=64` experiment; and
- a clean, single-process `CPULoad=128` reproduction during capture
  finalization.

The clean reproduction was decisive. It used a fresh XP boot, exactly one
interactive Capturing Tool process, and the original registry value. It still
crashed. The immediate fault mapped to `USBPORT.SYS+0x9d43` at elevated IRQL.

The dumps were useful because they ruled out two tempting stories:

- the crashes did not require multiple hidden application instances; and
- they did not require the experimental registry value.

But they also marked a decision point. More symbol work could perhaps
distinguish a stale request supplied by Sony from an XP USBPORT/QEMU OHCI
bug. It would not necessarily teach us how to receive the camera on Linux.

By then we already had:

- full USB descriptors;
- known-good and known-bad traces;
- a successful decoded Windows AVI;
- the cross-endpoint framing rule;
- the compressed record header;
- initialization traffic; and
- named JPEG routines and tables in the Sony driver.

The VM had served its purpose as an oracle. It did not need to become a
reliable production capture system.

## The first direct Linux probe

With the VM shut down and the camera returned to the host, Linux presented an
ideal split:

- vendor interface 0 had no kernel driver; and
- `snd-usb-audio` owned the standard audio interfaces.

The first `handycam-capture` prototype claimed only interface 0, selected alt
5, and scheduled 32 one-packet asynchronous transfers on each video endpoint.
The transport worked perfectly—but every ISO packet was empty.

That negative result was clean and informative. Selecting the bandwidth was
not the same as enabling the camera.

The Windows trace showed that video began shortly after audio interface 2 was
started, so we tried activating standard USB audio through ALSA while the
libusb probe ran. The video packets remained empty. Audio was not the missing
switch.

What remained was Sony's vendor request `0x88`.

## Replaying the initialization, not guessing it

Rather than assign speculative names to a hundred registers, we extracted the
known-good initialization block into a reviewable TSV plan. It contained:

- 140 vendor writes;
- 80 vendor reads;
- interface 0 alt 0, then alt 7, then alt 5; and
- 2.563 seconds of original inter-request timing.

Most operations were one byte. Several were 64-byte state blocks. Two were
immediately recognizable as JPEG quantization matrices.

The replayer was deliberately narrow. It accepted only Sony request `0x88`
and interface-0 `SET_INTERFACE`, checked every payload and return length, and
restored alt 0 on shutdown.

Then we ran it.

In five seconds, direct Linux capture produced:

```text
127 endpoint-0x81 boundary packets
125 endpoint-0x82 compressed headers
125 boundary/header pairs
25 headers in every one-second interval
zero ISO packet errors
```

That was the central breakthrough. No Windows driver, DirectShow component,
Sony application, or VM was involved. The protocol implementation itself was
receiving the camera.

## The JPEG hiding in plain sight

Each `0x82` record began:

```text
ff ff ff ff ff ff | two timestamp/flag bytes | compressed payload
```

The payload looked like JPEG entropy data, but tools did not recognize it as
a JPEG. That was expected once we looked carefully:

- there was no SOI marker;
- there were no quantization, frame, Huffman, or scan marker segments;
- `0xff` entropy bytes were not escaped with `0x00`; and
- the record ended with zero padding.

The control trace supplied the quantization matrices. They were the standard
JPEG quality-50 luma and chroma matrices, transposed for the camera's register
layout. The Sony driver contained standard-looking Huffman and inverse-DCT
routines.

We generated ordinary baseline JPEG headers for the plausible chroma
sampling modes and prefixed them to one Sony entropy record, adding byte
stuffing and EOI. The 4:4:4 and 4:2:2 variants produced decoder errors.
The 4:2:0 variant decoded without complaint.

And there it was: a correct 320x240 image from the camera, captured and
decoded entirely on Linux.

The decoder was then run over every bounded record. All 124 decoded. FFmpeg
accepted the complete sequence without errors.

One final analytical correction made the cadence cleaner than it first
appeared. We had initially treated the timestamp as wrapping at 2000 ms. The
field is 11 bits, so the real modulus is 2048. Two apparent backward jumps
were actually exact 40 ms transitions across wraparound. After excluding one
startup-discontinuity record, all 123 frames advanced by precisely 40 ms.

The resulting AVI was:

```text
MJPEG
320x240
25 fps
123 frames
4.92 seconds
```

## What actually mattered

Several habits made the difference.

First, we preserved failed artifacts. An audio-only AVI, a frozen-looking
successful AVI, and clean zero-length Linux probes each isolated a different
layer of the system.

Second, we compared layers instead of treating “capture” as one operation:

```text
camera generation
  -> USB transport
  -> endpoint ordering
  -> Sony framing
  -> JPEG reconstruction
  -> application/container output
```

The camera could be healthy while the Windows sample-delivery path failed.
USB packets could be valid while their cross-endpoint completion order broke
Sony's state machine. A compressed record could be a valid JPEG scan without
being a valid JPEG file.

Third, the VM was treated as a behavioral oracle, not the destination. Once
it had revealed the setup sequence, framing rule, and decoded output, making
XP perfectly stable was no longer necessary.

Finally, the implementation stayed conservative. It replayed known-good
control traffic before trying to explain every register. That produced a
working baseline from which semantic omission tests can now proceed safely.

## Where the project stands

The difficult uncertainty is gone. We can:

- initialize the camera directly with libusb;
- receive its two vendor isochronous endpoints;
- recognize every frame at 25 fps;
- reconstruct the Sony records;
- turn their raw entropy into standard JPEG; and
- mux the result as ordinary MJPEG video.

The remaining work is engineering:

- decode and emit frames live rather than in an offline pass;
- record standard USB audio through ALSA and synchronize it;
- reduce the 223-operation initialization plan;
- add persistent udev permissions;
- expose frames to GStreamer or `v4l2loopback`; and
- test longer sessions, reconnects, and tape playback.

That is a much better class of problem than “why does this obsolete XP
application sometimes freeze and sometimes blue-screen?” The camera's
protocol is no longer a black box. It is a small, testable state machine
followed by a very recognizable JPEG scan.

The implementation, evidence hashes, and exact reproduction commands live in
the project [README](../README.md), while the implementation-oriented wire
description is in [PROTOCOL.md](../PROTOCOL.md).

## Epilogue: the trace kept answering questions

Once capture worked, the preserved traffic continued to pay dividends
without the camera connected. Pairing every endpoint-zero submission and
completion turned `0x0340` from an opaque 64-byte blob into a small state
record. Its first byte acknowledges the sequence token placed at the end of
the `0x0300` mailbox, while two four-state bytes behave like briefly
diverging producer and consumer cursors. That is enough to replace part of
the recorded delay script with a bounded state poll.

The quality wizard trace settled another question. Sony did not upload new
JPEG quantization tables for its low-quality setting; those bytes stayed
identical. It changed register `0x007c` from 20 to 80 and continued adjusting
that value during capture. Quality selection is therefore a small,
independent control rather than a second codec format.

Finally, the apparently frozen AVI became affirmative evidence. In the
longest stored session, 192 reconstructed frames had 192 distinct hashes,
even though the Sony timestamp repeated for most of them. Endpoint `0x83`
provided exactly enough standard PCM to build a 9.040-second synchronized
Matroska sample, with every video PTS and the audio start derived from the USB
clock. The frozen picture belonged to the Windows timestamp path, not the
camera data.

Those discoveries changed the remaining task again. It is no longer “replay
223 mysterious operations forever.” It is a staged replacement exercise:
generate the named register phases, poll demonstrated state, add known
quality control, learn transport commands from differential playback traces,
and keep the original capture as a regression oracle throughout.
