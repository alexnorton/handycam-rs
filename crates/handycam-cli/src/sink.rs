use std::borrow::Cow;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::thread::{self, JoinHandle};

use handycam_core::{
    CompressedFrame, FPS, FrameConversionError, HEIGHT, OutputFormat, WIDTH, jpeg_to_yuyv,
};
use thiserror::Error;
use tracing::debug;
use v4l::Device;
use v4l::capability;
use v4l::format::{Colorspace, Format, FourCC, Quantization};
use v4l::video::Output;
use v4l::video::output::Parameters;

#[derive(Clone, Debug)]
pub enum SinkTarget {
    Stdout,
    V4l2(PathBuf),
}

#[derive(Debug)]
pub enum SinkStatus {
    Ready,
    Closed,
    Failed(String),
}

#[derive(Debug, Error)]
enum SinkError {
    #[error("frame conversion failed: {0}")]
    Conversion(#[from] FrameConversionError),
    #[error("output I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("V4L2 node does not support single-planar video output and read/write I/O")]
    NotOutputDevice,
    #[error("V4L2 rejected requested format; received {width}x{height} {fourcc}")]
    FormatRejected {
        width: u32,
        height: u32,
        fourcc: FourCC,
    },
    #[error("V4L2 accepted only {actual} of {required} frame bytes")]
    ShortWrite { actual: usize, required: usize },
}

pub fn spawn(
    target: SinkTarget,
    format: OutputFormat,
    frames: Receiver<CompressedFrame>,
    status: Sender<SinkStatus>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("handycam-output".to_owned())
        .spawn(move || {
            let result = match target {
                SinkTarget::Stdout => run_stdout(format, frames, &status),
                SinkTarget::V4l2(path) => run_v4l2(path, format, frames, &status),
            };
            match result {
                Ok(()) => {
                    let _ = status.send(SinkStatus::Closed);
                }
                Err(SinkError::Io(error)) if error.kind() == io::ErrorKind::BrokenPipe => {
                    let _ = status.send(SinkStatus::Closed);
                }
                Err(error) => {
                    let _ = status.send(SinkStatus::Failed(error.to_string()));
                }
            }
        })
        .expect("could not spawn output worker")
}

fn frame_data(
    frame: &CompressedFrame,
    format: OutputFormat,
) -> Result<Cow<'_, [u8]>, FrameConversionError> {
    match format {
        OutputFormat::Mjpeg => Ok(Cow::Borrowed(&frame.jpeg)),
        OutputFormat::Yuyv => Ok(Cow::Owned(jpeg_to_yuyv(&frame.jpeg)?)),
    }
}

fn run_stdout(
    format: OutputFormat,
    frames: Receiver<CompressedFrame>,
    status: &Sender<SinkStatus>,
) -> Result<(), SinkError> {
    let stdout = io::stdout();
    let mut output = BufWriter::with_capacity(16 * 1024, stdout.lock());
    status.send(SinkStatus::Ready).ok();
    for frame in frames {
        let data = frame_data(&frame, format)?;
        output.write_all(&data)?;
        output.flush()?;
    }
    output.flush()?;
    Ok(())
}

fn run_v4l2(
    path: PathBuf,
    output_format: OutputFormat,
    frames: Receiver<CompressedFrame>,
    status: &Sender<SinkStatus>,
) -> Result<(), SinkError> {
    let mut device = Device::with_path(&path)?;
    let capabilities = device.query_caps()?;
    let required = capability::Flags::VIDEO_OUTPUT | capability::Flags::READ_WRITE;
    if !capabilities.capabilities.contains(required) {
        return Err(SinkError::NotOutputDevice);
    }

    let fourcc = match output_format {
        OutputFormat::Mjpeg => FourCC::new(b"MJPG"),
        OutputFormat::Yuyv => FourCC::new(b"YUYV"),
    };
    let mut requested = Format::new(WIDTH, HEIGHT, fourcc);
    match output_format {
        OutputFormat::Mjpeg => {
            requested.size = 256 * 1024;
            requested.colorspace = Colorspace::JPEG;
            requested.quantization = Quantization::FullRange;
        }
        OutputFormat::Yuyv => {
            requested.stride = WIDTH * 2;
            requested.size = WIDTH * HEIGHT * 2;
            requested.colorspace = Colorspace::SMPTE170M;
            requested.quantization = Quantization::LimitedRange;
        }
    }
    let accepted = Output::set_format(&device, &requested)?;
    if accepted.width != WIDTH || accepted.height != HEIGHT || accepted.fourcc != fourcc {
        return Err(SinkError::FormatRejected {
            width: accepted.width,
            height: accepted.height,
            fourcc: accepted.fourcc,
        });
    }
    let accepted_params = Output::set_params(&device, &Parameters::with_fps(FPS))?;
    debug!(
        path = %path.display(),
        format = %fourcc,
        interval = %accepted_params.interval,
        "configured V4L2 output"
    );

    status.send(SinkStatus::Ready).ok();
    for frame in frames {
        let data = frame_data(&frame, output_format)?;
        let written = device.write(&data)?;
        if written != data.len() {
            return Err(SinkError::ShortWrite {
                actual: written,
                required: data.len(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mjpeg_stdout_data_has_no_extra_framing() {
        let frame = CompressedFrame {
            sequence: 0,
            timestamp_11bit_ms: 0,
            timestamp_unwrapped_ms: 0,
            timestamp_delta_to_next_ms: 40,
            flags: 0,
            jpeg: vec![0xff, 0xd8, 1, 2, 0xff, 0xd9],
        };
        assert_eq!(
            frame_data(&frame, OutputFormat::Mjpeg).unwrap().as_ref(),
            frame.jpeg
        );
    }
}
