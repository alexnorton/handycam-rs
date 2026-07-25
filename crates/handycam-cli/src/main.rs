mod sink;

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use handycam_core::{
    CompressedFrame, EndpointPacket, InitOp, OutputFormat, ScaleFactor, StreamDecoder,
    TransportCommand, TransportCommandEncoder, override_record_mode_scale_factor, parse_init_plan,
    record_mode_init_plan,
};
use handycam_libusb::{
    CameraSelector, InitStrategy, SessionEvent, UsbSession, UsbTransportError, read_camera_status,
    send_transport_command,
};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use thiserror::Error;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::sink::{SinkStatus, SinkTarget};

#[derive(Parser, Debug)]
#[command(
    name = "handycam",
    version,
    about = "Userspace driver for Sony DCR-HC24 USB video"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Stream live camera video to V4L2 or stdout.
    Stream(StreamArgs),
    /// Replay reconstructed JPEG frames through the production output sink.
    Replay(ReplayArgs),
    /// Replay an extracted endpoint capture through the protocol decoder.
    ReplayCapture(ReplayCaptureArgs),
    /// Experimentally inspect status or send one playback transport command.
    Transport(TransportArgs),
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum FormatArg {
    #[default]
    Mjpeg,
    Yuyv,
}

impl From<FormatArg> for OutputFormat {
    fn from(format: FormatArg) -> Self {
        match format {
            FormatArg::Mjpeg => Self::Mjpeg,
            FormatArg::Yuyv => Self::Yuyv,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum LogFormat {
    #[default]
    Text,
    Json,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum TransportAction {
    Status,
    Stop,
    Play,
    Pause,
    Rewind,
    FastForward,
}

impl TransportAction {
    fn command(self) -> Option<TransportCommand> {
        match self {
            Self::Status => None,
            Self::Stop => Some(TransportCommand::Stop),
            Self::Play => Some(TransportCommand::Play),
            Self::Pause => Some(TransportCommand::Pause),
            Self::Rewind => Some(TransportCommand::Rewind),
            Self::FastForward => Some(TransportCommand::FastForward),
        }
    }
}

#[derive(Parser, Debug)]
struct TransportArgs {
    /// Read status or send one recovered tape-transport operation.
    #[arg(value_enum)]
    action: TransportAction,

    /// Select a physical USB path such as 001-2.3.
    #[arg(long)]
    usb_path: Option<CameraSelector>,

    /// Four-bit sequence value before the command; Sony increments it first.
    #[arg(long)]
    current_sequence: Option<u8>,

    /// Delay before reading status after a command.
    #[arg(long, default_value_t = 500)]
    settle_ms: u64,

    /// Print the encoded command without touching USB.
    #[arg(long)]
    dry_run: bool,

    /// Diagnostic log representation; logs always go to stderr.
    #[arg(long, value_enum, default_value_t)]
    log_format: LogFormat,

    /// Increase diagnostic verbosity.
    #[arg(short, long, action = ArgAction::Count)]
    verbose: u8,
}

#[derive(Parser, Debug)]
struct StreamArgs {
    /// V4L2 output node, or - for stdout.
    #[arg(long)]
    output: String,

    /// Output pixel/stream format.
    #[arg(long, value_enum, default_value_t)]
    format: FormatArg,

    /// Select a physical USB path such as 001-2.3.
    #[arg(long)]
    usb_path: Option<CameraSelector>,

    /// Override the embedded record-mode initialization plan.
    #[arg(long)]
    init_plan: Option<PathBuf>,

    /// JPEG scale factor 4..=128; larger values produce lower quality.
    #[arg(long)]
    quality: Option<u8>,

    /// Replace four captured startup read-runs with status-token polling.
    #[arg(long, conflicts_with = "playback_init")]
    semantic_init: bool,

    /// Poll playback-mode n9 startup acknowledgements instead of literal reads.
    #[arg(long, conflicts_with = "semantic_init")]
    playback_init: bool,

    /// Exit rather than waiting for or reconnecting the camera.
    #[arg(long)]
    no_reconnect: bool,

    /// Diagnostic log representation; logs always go to stderr.
    #[arg(long, value_enum, default_value_t)]
    log_format: LogFormat,

    /// Increase diagnostic verbosity.
    #[arg(short, long, action = ArgAction::Count)]
    verbose: u8,
}

#[derive(Parser, Debug)]
struct ReplayArgs {
    /// JPEG file or directory of JPEG frames, sorted by filename.
    #[arg(long)]
    input: PathBuf,

    /// V4L2 output node, or - for stdout.
    #[arg(long)]
    output: String,

    /// Output pixel/stream format.
    #[arg(long, value_enum, default_value_t)]
    format: FormatArg,

    /// Repeat the input until interrupted.
    #[arg(long = "loop")]
    repeat: bool,

    /// Diagnostic log representation; logs always go to stderr.
    #[arg(long, value_enum, default_value_t)]
    log_format: LogFormat,

    /// Increase diagnostic verbosity.
    #[arg(short, long, action = ArgAction::Count)]
    verbose: u8,
}

#[derive(Parser, Debug)]
struct ReplayCaptureArgs {
    /// Directory containing iso-packets.jsonl, ep81.bin, and ep82.bin.
    #[arg(long)]
    input: PathBuf,

    /// V4L2 output node, or - for stdout.
    #[arg(long)]
    output: String,

    /// Output pixel/stream format.
    #[arg(long, value_enum, default_value_t)]
    format: FormatArg,

    /// Pace decoded frames at the camera's nominal 25 fps.
    #[arg(long)]
    realtime: bool,

    /// Diagnostic log representation; logs always go to stderr.
    #[arg(long, value_enum, default_value_t)]
    log_format: LogFormat,

    /// Increase diagnostic verbosity.
    #[arg(short, long, action = ArgAction::Count)]
    verbose: u8,
}

#[derive(Debug, Error)]
enum AppError {
    #[error("could not read initialization plan {path}: {source}")]
    ReadPlan {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid initialization plan: {0}")]
    Plan(#[from] handycam_core::InitPlanError),
    #[error(transparent)]
    ScaleFactor(#[from] handycam_core::ScaleFactorError),
    #[error("initialization plan has no configured scale-factor write at index 0x007c")]
    MissingScaleFactorWrite,
    #[error("could not install signal handler: {0}")]
    Signal(#[from] std::io::Error),
    #[error("could not read replay input {path}: {source}")]
    ReadReplay {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("replay input {0} contains no JPEG files")]
    NoReplayFrames(PathBuf),
    #[error("replay input {0} is not a complete JPEG image")]
    InvalidReplayFrame(PathBuf),
    #[error("invalid extracted-capture metadata at line {line}: {message}")]
    ReplayMetadata { line: usize, message: String },
    #[error("extracted endpoint {endpoint} range {offset}..{end} exceeds its {length}-byte file")]
    ReplayRange {
        endpoint: String,
        offset: usize,
        end: usize,
        length: usize,
    },
    #[error("output sink failed: {0}")]
    Sink(String),
    #[error("output sink stopped")]
    SinkStopped,
    #[error("USB camera failed: {0}")]
    Usb(#[from] UsbTransportError),
    #[error("video protocol failed: {0}")]
    Protocol(#[from] handycam_core::StreamDecodeError),
    #[error("output worker panicked")]
    SinkPanicked,
    #[error("transport sequence {0} is outside the four-bit range 0..=15")]
    InvalidTransportSequence(u8),
}

#[derive(Default)]
struct DriverStats {
    boundaries: u64,
    headers: u64,
    frames: u64,
    timestamp_gaps: u64,
    queue_drops: u64,
    protocol_errors: u64,
    reconnects: u64,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match &cli.command {
        Command::Stream(arguments) => {
            initialize_logging(arguments.log_format, arguments.verbose);
        }
        Command::Replay(arguments) => {
            initialize_logging(arguments.log_format, arguments.verbose);
        }
        Command::ReplayCapture(arguments) => {
            initialize_logging(arguments.log_format, arguments.verbose);
        }
        Command::Transport(arguments) => {
            initialize_logging(arguments.log_format, arguments.verbose);
        }
    }
    let result = match cli.command {
        Command::Stream(arguments) => run_stream(arguments),
        Command::Replay(arguments) => run_replay(arguments),
        Command::ReplayCapture(arguments) => run_replay_capture(arguments),
        Command::Transport(arguments) => run_transport(arguments),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            error!(%error);
            ExitCode::FAILURE
        }
    }
}

fn format_status(status: &[u8]) -> String {
    status
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn run_transport(arguments: TransportArgs) -> Result<(), AppError> {
    if let Some(sequence) = arguments.current_sequence
        && sequence > 0x0f
    {
        return Err(AppError::InvalidTransportSequence(sequence));
    }
    let selector = arguments.usb_path.unwrap_or_default();
    let Some(command) = arguments.action.command() else {
        let status = read_camera_status(&selector)?;
        println!("status={}", format_status(&status));
        return Ok(());
    };

    if arguments.dry_run {
        let mut encoder =
            TransportCommandEncoder::with_sequence(arguments.current_sequence.unwrap_or(0));
        let word = encoder.encode(command);
        println!(
            "command={:?} word={word:08x} sequence={}",
            arguments.action,
            encoder.sequence()
        );
        return Ok(());
    }

    let observation = send_transport_command(
        &selector,
        command,
        arguments.current_sequence,
        Duration::from_millis(arguments.settle_ms),
    )?;
    let changes = observation
        .status_before
        .iter()
        .zip(&observation.status_after)
        .enumerate()
        .filter(|(_, (before, after))| before != after)
        .map(|(offset, (before, after))| format!("{offset}:{before:02x}->{after:02x}"))
        .collect::<Vec<_>>();
    println!(
        "command={:?} word={:08x} sequence={} usb={:03}-{} status_before={} status_after={} changes={}",
        arguments.action,
        observation.command_word,
        observation.sequence,
        observation.bus,
        observation
            .port_path
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join("."),
        format_status(&observation.status_before),
        format_status(&observation.status_after),
        if changes.is_empty() {
            "none".to_owned()
        } else {
            changes.join(",")
        }
    );
    Ok(())
}

fn initialize_logging(log_format: LogFormat, verbose: u8) {
    let default_filter = match verbose {
        0 => "handycam=info,handycam_libusb=info",
        1 => "handycam=debug,handycam_libusb=debug",
        _ => "debug",
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    match log_format {
        LogFormat::Text => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .with_target(false)
            .init(),
        LogFormat::Json => tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .with_target(false)
            .init(),
    }
}

fn run_replay(arguments: ReplayArgs) -> Result<(), AppError> {
    let jpeg_frames = load_replay_frames(&arguments.input)?;
    let output_format = OutputFormat::from(arguments.format);
    let target = if arguments.output == "-" {
        SinkTarget::Stdout
    } else {
        SinkTarget::V4l2(PathBuf::from(&arguments.output))
    };
    let stopping = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGINT, Arc::clone(&stopping))?;
    signal_hook::flag::register(SIGTERM, Arc::clone(&stopping))?;

    let (frame_sender, frame_receiver) = mpsc::sync_channel(1);
    let (sink_status_sender, sink_status_receiver) = mpsc::channel();
    let sink_thread = sink::spawn(target, output_format, frame_receiver, sink_status_sender);
    wait_for_sink(&sink_status_receiver)?;

    info!(
        frames = jpeg_frames.len(),
        repeat = arguments.repeat,
        "starting recorded-frame replay"
    );
    let frame_interval = Duration::from_micros(1_000_000 / u64::from(handycam_core::FPS));
    let mut sequence = 0_u64;
    let mut deadline = Instant::now();
    let mut sink_error = None;
    'replay: loop {
        for jpeg in &jpeg_frames {
            if stopping.load(Ordering::Relaxed) {
                break 'replay;
            }
            if let Some(result) = check_sink(&sink_status_receiver) {
                if let Err(error) = result {
                    sink_error = Some(error);
                }
                break 'replay;
            }
            let timestamp = ((sequence * 40) & 0x07ff) as u16;
            let frame = CompressedFrame {
                sequence,
                timestamp_11bit_ms: timestamp,
                timestamp_unwrapped_ms: sequence * 40,
                timestamp_delta_to_next_ms: 40,
                flags: 0,
                jpeg: jpeg.clone(),
            };
            match frame_sender.try_send(frame) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => break 'replay,
            }
            sequence += 1;
            deadline += frame_interval;
            while Instant::now() < deadline {
                if stopping.load(Ordering::Relaxed) {
                    break 'replay;
                }
                thread::sleep((deadline - Instant::now()).min(Duration::from_millis(10)));
            }
        }
        if !arguments.repeat {
            break;
        }
    }

    drop(frame_sender);
    if sink_thread.join().is_err() {
        return Err(AppError::SinkPanicked);
    }
    while let Ok(status) = sink_status_receiver.try_recv() {
        if let SinkStatus::Failed(error) = status {
            sink_error = Some(AppError::Sink(error));
        }
    }
    if let Some(error) = sink_error {
        return Err(error);
    }
    info!(frames = sequence, "recorded-frame replay stopped");
    Ok(())
}

fn load_replay_frames(path: &Path) -> Result<Vec<Vec<u8>>, AppError> {
    let mut paths = if path.is_dir() {
        let entries = fs::read_dir(path).map_err(|source| AppError::ReadReplay {
            path: path.to_path_buf(),
            source,
        })?;
        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| AppError::ReadReplay {
                path: path.to_path_buf(),
                source,
            })?;
            let entry_path = entry.path();
            if entry_path.is_file()
                && entry_path.extension().is_some_and(|extension| {
                    extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg")
                })
            {
                paths.push(entry_path);
            }
        }
        paths.sort();
        paths
    } else {
        vec![path.to_path_buf()]
    };
    if paths.is_empty() {
        return Err(AppError::NoReplayFrames(path.to_path_buf()));
    }

    let mut frames = Vec::with_capacity(paths.len());
    for frame_path in paths.drain(..) {
        let jpeg = fs::read(&frame_path).map_err(|source| AppError::ReadReplay {
            path: frame_path.clone(),
            source,
        })?;
        if !is_complete_jpeg(&jpeg) {
            return Err(AppError::InvalidReplayFrame(frame_path));
        }
        frames.push(jpeg);
    }
    Ok(frames)
}

fn is_complete_jpeg(data: &[u8]) -> bool {
    data.starts_with(&[0xff, 0xd8]) && data.ends_with(&[0xff, 0xd9])
}

fn metadata_usize(
    row: &serde_json::Value,
    field: &'static str,
    line: usize,
) -> Result<usize, AppError> {
    row[field]
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| AppError::ReplayMetadata {
            line,
            message: format!("{field} is missing or is not an unsigned integer"),
        })
}

fn run_replay_capture(arguments: ReplayCaptureArgs) -> Result<(), AppError> {
    let metadata_path = arguments.input.join("iso-packets.jsonl");
    let metadata = fs::File::open(&metadata_path).map_err(|source| AppError::ReadReplay {
        path: metadata_path.clone(),
        source,
    })?;
    let ep81_path = arguments.input.join("ep81.bin");
    let ep82_path = arguments.input.join("ep82.bin");
    let ep81 = fs::read(&ep81_path).map_err(|source| AppError::ReadReplay {
        path: ep81_path,
        source,
    })?;
    let ep82 = fs::read(&ep82_path).map_err(|source| AppError::ReadReplay {
        path: ep82_path,
        source,
    })?;

    let output_format = OutputFormat::from(arguments.format);
    let target = if arguments.output == "-" {
        SinkTarget::Stdout
    } else {
        SinkTarget::V4l2(PathBuf::from(&arguments.output))
    };
    let (frame_sender, frame_receiver) = mpsc::sync_channel(1);
    let (sink_status_sender, sink_status_receiver) = mpsc::channel();
    let sink_thread = sink::spawn(target, output_format, frame_receiver, sink_status_sender);
    wait_for_sink(&sink_status_receiver)?;

    let mut decoder = StreamDecoder::new();
    let mut frames = 0_u64;
    let mut packets = 0_u64;
    let mut errors = 0_u64;
    let frame_interval = Duration::from_micros(1_000_000 / u64::from(handycam_core::FPS));
    let mut deadline = Instant::now();
    for (offset, line_result) in BufReader::new(metadata).lines().enumerate() {
        let line_number = offset + 1;
        let line = line_result.map_err(|source| AppError::ReadReplay {
            path: metadata_path.clone(),
            source,
        })?;
        let row: serde_json::Value =
            serde_json::from_str(&line).map_err(|error| AppError::ReplayMetadata {
                line: line_number,
                message: error.to_string(),
            })?;
        let endpoint_name = row["endpoint"]
            .as_str()
            .ok_or_else(|| AppError::ReplayMetadata {
                line: line_number,
                message: "endpoint is missing or is not a string".to_owned(),
            })?;
        let (endpoint, source) = match endpoint_name {
            "0x81" => (handycam_core::Endpoint::Boundary, ep81.as_slice()),
            "0x82" => (handycam_core::Endpoint::Video, ep82.as_slice()),
            _ => continue,
        };
        let length = metadata_usize(&row, "length", line_number)?;
        if length == 0 {
            continue;
        }
        let data_offset = metadata_usize(&row, "data_offset", line_number)?;
        let end = data_offset
            .checked_add(length)
            .ok_or_else(|| AppError::ReplayMetadata {
                line: line_number,
                message: "data range overflows usize".to_owned(),
            })?;
        let data = source
            .get(data_offset..end)
            .ok_or_else(|| AppError::ReplayRange {
                endpoint: endpoint_name.to_owned(),
                offset: data_offset,
                end,
                length: source.len(),
            })?;
        packets += 1;
        match decoder.push_packet(EndpointPacket { endpoint, data }) {
            Ok(Some(frame)) => {
                if frame_sender.send(frame).is_err() {
                    break;
                }
                frames += 1;
                if arguments.realtime {
                    deadline += frame_interval;
                    while Instant::now() < deadline {
                        thread::sleep((deadline - Instant::now()).min(Duration::from_millis(10)));
                    }
                }
            }
            Ok(None) => {}
            Err(error) => {
                errors += 1;
                warn!(line = line_number, %error, "discarding malformed replay record");
                decoder.reset();
            }
        }
    }

    drop(frame_sender);
    if sink_thread.join().is_err() {
        return Err(AppError::SinkPanicked);
    }
    while let Ok(status) = sink_status_receiver.try_recv() {
        if let SinkStatus::Failed(error) = status {
            return Err(AppError::Sink(error));
        }
    }
    info!(
        packets,
        frames,
        protocol_errors = errors,
        "raw capture replay stopped"
    );
    Ok(())
}

fn run_stream(arguments: StreamArgs) -> Result<(), AppError> {
    let mut plan = load_plan(arguments.init_plan.as_ref())?;
    if let Some(value) = arguments.quality {
        let scale_factor = ScaleFactor::new(value)?;
        if !override_record_mode_scale_factor(&mut plan, scale_factor) {
            return Err(AppError::MissingScaleFactorWrite);
        }
        info!(quality = value, "overrode JPEG scale factor");
    }
    let init_strategy = if arguments.semantic_init {
        InitStrategy::ConditionPolling
    } else if arguments.playback_init {
        InitStrategy::PlaybackConditionPolling
    } else {
        InitStrategy::LiteralReplay
    };
    let selector = arguments.usb_path.unwrap_or_default();
    let output_format = OutputFormat::from(arguments.format);
    let target = if arguments.output == "-" {
        SinkTarget::Stdout
    } else {
        SinkTarget::V4l2(PathBuf::from(&arguments.output))
    };

    let stopping = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGINT, Arc::clone(&stopping))?;
    signal_hook::flag::register(SIGTERM, Arc::clone(&stopping))?;

    let (frame_sender, frame_receiver) = mpsc::sync_channel(1);
    let (sink_status_sender, sink_status_receiver) = mpsc::channel();
    let sink_thread = sink::spawn(target, output_format, frame_receiver, sink_status_sender);
    wait_for_sink(&sink_status_receiver)?;

    let mut stats = DriverStats::default();
    let mut retry_delay = Duration::from_millis(500);
    let result = loop {
        if stopping.load(Ordering::Relaxed) {
            break Ok(());
        }
        if let Some(result) = check_sink(&sink_status_receiver) {
            break result;
        }

        info!(selector = %selector, "opening camera");
        let mut session = match UsbSession::open_with_init_strategy(&selector, &plan, init_strategy)
        {
            Ok(session) => session,
            Err(error) if should_retry_open(&error) && !arguments.no_reconnect => {
                warn!(%error, retry_ms = retry_delay.as_millis(), "camera unavailable");
                if wait_interruptibly(retry_delay, &stopping, &sink_status_receiver)? {
                    break Ok(());
                }
                retry_delay = (retry_delay * 2).min(Duration::from_secs(5));
                continue;
            }
            Err(error) => break Err(error.into()),
        };
        retry_delay = Duration::from_millis(500);
        stats.reconnects += 1;
        info!(
            bus = session.bus(),
            address = session.address(),
            ports = ?session.port_path(),
            "camera initialized"
        );

        let mut decoder = StreamDecoder::new();
        let session_result = run_session(
            &mut session,
            &mut decoder,
            &frame_sender,
            &sink_status_receiver,
            &stopping,
            &mut stats,
        );
        session.stop();
        match session_result {
            Ok(SessionEnd::Stopped) => break Ok(()),
            Ok(SessionEnd::SinkClosed) => break Ok(()),
            Ok(SessionEnd::Disconnected(reason)) => {
                warn!(%reason, "camera session ended");
                if arguments.no_reconnect {
                    break Err(AppError::Sink(format!("camera disconnected: {reason}")));
                }
            }
            Err(error) => break Err(error),
        }
    };

    drop(frame_sender);
    if sink_thread.join().is_err() {
        return Err(AppError::SinkPanicked);
    }
    result
}

enum SessionEnd {
    Stopped,
    SinkClosed,
    Disconnected(String),
}

fn is_nominal_camera_delta(delta_ms: u16) -> bool {
    (39..=41).contains(&delta_ms)
}

fn run_session(
    session: &mut UsbSession,
    decoder: &mut StreamDecoder,
    frames: &SyncSender<CompressedFrame>,
    sink_status: &mpsc::Receiver<SinkStatus>,
    stopping: &AtomicBool,
    stats: &mut DriverStats,
) -> Result<SessionEnd, AppError> {
    let mut next_report = Instant::now() + Duration::from_secs(10);
    loop {
        if stopping.load(Ordering::Relaxed) {
            return Ok(SessionEnd::Stopped);
        }
        if let Some(result) = check_sink(sink_status) {
            return match result {
                Ok(()) => Ok(SessionEnd::SinkClosed),
                Err(error) => Err(error),
            };
        }
        let events = match session.poll(Duration::from_millis(100)) {
            Ok(events) => events,
            Err(error) => {
                if stopping.load(Ordering::Relaxed) {
                    return Ok(SessionEnd::Stopped);
                }
                return Ok(SessionEnd::Disconnected(error.to_string()));
            }
        };
        for event in events {
            match event {
                SessionEvent::Packet { endpoint, data, .. } => {
                    match endpoint {
                        handycam_core::Endpoint::Boundary => {
                            if data.first().is_some_and(|byte| byte & 0x08 != 0) {
                                stats.boundaries += 1;
                            }
                        }
                        handycam_core::Endpoint::Video => {
                            if data.len() >= 8 && data[..6] == [0xff; 6] {
                                stats.headers += 1;
                            }
                        }
                    }
                    match decoder.push_packet(EndpointPacket {
                        endpoint,
                        data: &data,
                    }) {
                        Ok(Some(frame)) => {
                            if !is_nominal_camera_delta(frame.timestamp_delta_to_next_ms) {
                                stats.timestamp_gaps += 1;
                                warn!(
                                    sequence = frame.sequence,
                                    delta_ms = frame.timestamp_delta_to_next_ms,
                                    "camera timestamp gap"
                                );
                            }
                            match frames.try_send(frame) {
                                Ok(()) => stats.frames += 1,
                                Err(TrySendError::Full(_)) => {
                                    stats.queue_drops += 1;
                                }
                                Err(TrySendError::Disconnected(_)) => {
                                    return Ok(SessionEnd::SinkClosed);
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            stats.protocol_errors += 1;
                            warn!(%error, "discarding malformed camera record");
                            decoder.reset();
                        }
                    }
                }
                SessionEvent::PacketError { endpoint, status } => {
                    warn!(?endpoint, status, "isochronous packet error");
                    decoder.reset();
                }
                event if event.ends_session() => {
                    return Ok(SessionEnd::Disconnected(format!("{event:?}")));
                }
                _ => {}
            }
        }
        if Instant::now() >= next_report {
            let usb = session.stats();
            info!(
                usb_packets = usb.packets,
                usb_nonempty = usb.nonempty_packets,
                usb_packet_errors = usb.packet_errors,
                usb_bytes = usb.bytes,
                boundaries = stats.boundaries,
                headers = stats.headers,
                frames = stats.frames,
                timestamp_gaps = stats.timestamp_gaps,
                queue_drops = stats.queue_drops,
                protocol_errors = stats.protocol_errors,
                "stream statistics"
            );
            next_report = Instant::now() + Duration::from_secs(10);
        }
    }
}

fn load_plan(path: Option<&PathBuf>) -> Result<Vec<InitOp>, AppError> {
    match path {
        None => Ok(record_mode_init_plan()?),
        Some(path) => {
            let text = fs::read_to_string(path).map_err(|source| AppError::ReadPlan {
                path: path.clone(),
                source,
            })?;
            Ok(parse_init_plan(&text)?)
        }
    }
}

fn wait_for_sink(statuses: &mpsc::Receiver<SinkStatus>) -> Result<(), AppError> {
    match statuses.recv() {
        Ok(SinkStatus::Ready) => Ok(()),
        Ok(SinkStatus::Failed(error)) => Err(AppError::Sink(error)),
        Ok(SinkStatus::Closed) | Err(_) => Err(AppError::SinkStopped),
    }
}

fn check_sink(statuses: &mpsc::Receiver<SinkStatus>) -> Option<Result<(), AppError>> {
    match statuses.try_recv() {
        Ok(SinkStatus::Ready) => None,
        Ok(SinkStatus::Closed) => Some(Ok(())),
        Ok(SinkStatus::Failed(error)) => Some(Err(AppError::Sink(error))),
        Err(mpsc::TryRecvError::Empty) => None,
        Err(mpsc::TryRecvError::Disconnected) => Some(Err(AppError::SinkStopped)),
    }
}

fn should_retry_open(error: &UsbTransportError) -> bool {
    matches!(
        error,
        UsbTransportError::NoCamera
            | UsbTransportError::Usb(rusb::Error::NoDevice)
            | UsbTransportError::Usb(rusb::Error::Io)
            | UsbTransportError::Usb(rusb::Error::Timeout)
    )
}

fn wait_interruptibly(
    duration: Duration,
    stopping: &AtomicBool,
    sink_status: &mpsc::Receiver<SinkStatus>,
) -> Result<bool, AppError> {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        if stopping.load(Ordering::Relaxed) {
            return Ok(true);
        }
        if let Some(result) = check_sink(sink_status) {
            result?;
            return Ok(true);
        }
        thread::sleep(Duration::from_millis(50));
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_only_complete_jpeg_images() {
        assert!(is_complete_jpeg(&[0xff, 0xd8, 1, 2, 0xff, 0xd9]));
        assert!(!is_complete_jpeg(&[0xff, 0xd8, 1, 2]));
        assert!(!is_complete_jpeg(&[1, 2, 0xff, 0xd9]));
        assert!(!is_complete_jpeg(&[]));
    }

    #[test]
    fn accepts_record_and_playback_timestamp_rounding() {
        assert!(!is_nominal_camera_delta(38));
        assert!(is_nominal_camera_delta(39));
        assert!(is_nominal_camera_delta(40));
        assert!(is_nominal_camera_delta(41));
        assert!(!is_nominal_camera_delta(42));
    }
}
