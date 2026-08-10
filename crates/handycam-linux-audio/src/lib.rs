//! Linux audio capture through direct ALSA or the `arecord` fallback frontend.

use std::io::{self, Read};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use alsa::Direction;
use alsa::pcm::{Access, Format, HwParams, PCM, TstampType};
use handycam_core::{
    AUDIO_BYTES_PER_SAMPLE_FRAME, AUDIO_CHANNELS, AUDIO_SAMPLE_RATE_HZ, AudioClock, AudioFormat,
    MonotonicTimestamp, TimedAudioChunk, TimestampAccuracy,
};
use thiserror::Error;

const PERIOD_SAMPLE_FRAMES: usize = 640;
const PERIOD_BYTES: usize = PERIOD_SAMPLE_FRAMES * AUDIO_BYTES_PER_SAMPLE_FRAME;

#[derive(Debug)]
pub enum AudioCaptureEvent {
    Chunk(TimedAudioChunk),
    Failed(String),
    Closed,
}

#[derive(Debug, Error)]
pub enum AudioCaptureError {
    #[error("could not start arecord: {0}")]
    Spawn(#[source] io::Error),
    #[error("arecord stdout was not piped")]
    MissingStdout,
    #[error("audio capture worker panicked")]
    WorkerPanicked,
    #[error("ALSA operation failed: {0}")]
    Alsa(#[from] alsa::Error),
    #[error("ALSA selected {actual} Hz instead of the required {expected} Hz")]
    UnexpectedRate { expected: u32, actual: u32 },
    #[error("ALSA selected a period of {actual} frames instead of the required {expected}")]
    UnexpectedPeriod { expected: i64, actual: i64 },
}

pub struct AlsaCapture {
    running: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl AlsaCapture {
    pub fn spawn(
        device: &str,
        session_origin: Instant,
    ) -> Result<(Self, Receiver<AudioCaptureEvent>), AudioCaptureError> {
        let pcm = configure_pcm(device)?;
        let monotonic_origin_ns = estimate_monotonic_origin_ns(session_origin);
        let running = Arc::new(AtomicBool::new(true));
        let worker_running = Arc::clone(&running);
        let (sender, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("handycam-alsa".to_owned())
            .spawn(move || run_alsa_capture(pcm, monotonic_origin_ns, worker_running, sender))
            .map_err(AudioCaptureError::Spawn)?;
        Ok((
            Self {
                running,
                worker: Some(worker),
            },
            receiver,
        ))
    }

    pub fn stop(mut self) -> Result<(), AudioCaptureError> {
        self.stop_inner()
    }

    fn stop_inner(&mut self) -> Result<(), AudioCaptureError> {
        self.running.store(false, Ordering::Release);
        if self
            .worker
            .take()
            .is_some_and(|worker| worker.join().is_err())
        {
            return Err(AudioCaptureError::WorkerPanicked);
        }
        Ok(())
    }
}

impl Drop for AlsaCapture {
    fn drop(&mut self) {
        let _ = self.stop_inner();
    }
}

pub struct ArecordCapture {
    child: Child,
    worker: Option<JoinHandle<()>>,
}

impl ArecordCapture {
    pub fn spawn(
        device: &str,
        session_origin: Instant,
    ) -> Result<(Self, Receiver<AudioCaptureEvent>), AudioCaptureError> {
        let mut child = Command::new("arecord")
            .args([
                "--quiet",
                "--fatal-errors",
                "--file-type=raw",
                "--format=S16_LE",
                "--rate=16000",
                "--channels=2",
                "--period-time=40000",
                "--buffer-time=160000",
                "--device",
                device,
                "-",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(AudioCaptureError::Spawn)?;
        let stdout = child
            .stdout
            .take()
            .ok_or(AudioCaptureError::MissingStdout)?;
        let (sender, receiver) = mpsc::channel();
        let worker = match thread::Builder::new()
            .name("handycam-audio".to_owned())
            .spawn(move || run_capture(stdout, session_origin, sender))
        {
            Ok(worker) => worker,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(AudioCaptureError::Spawn(error));
            }
        };
        Ok((
            Self {
                child,
                worker: Some(worker),
            },
            receiver,
        ))
    }

    pub fn stop(mut self) -> Result<(), AudioCaptureError> {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if self
            .worker
            .take()
            .is_some_and(|worker| worker.join().is_err())
        {
            return Err(AudioCaptureError::WorkerPanicked);
        }
        Ok(())
    }
}

impl Drop for ArecordCapture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run_capture(mut input: impl Read, session_origin: Instant, sender: Sender<AudioCaptureEvent>) {
    let format = AudioFormat::pcm_s16le(AUDIO_SAMPLE_RATE_HZ, u32::from(AUDIO_CHANNELS))
        .expect("the camera audio format is valid");
    let mut clock = AudioClock::new();
    loop {
        let mut pcm = vec![0; PERIOD_BYTES];
        if let Err(error) = input.read_exact(&mut pcm) {
            let event = if error.kind() == io::ErrorKind::UnexpectedEof {
                AudioCaptureEvent::Closed
            } else {
                AudioCaptureEvent::Failed(error.to_string())
            };
            let _ = sender.send(event);
            return;
        }
        let observed_end = Instant::now();
        let observed_start = observed_end
            .checked_sub(Duration::from_millis(40))
            .unwrap_or(observed_end);
        let observed_us = observed_start
            .checked_duration_since(session_origin)
            .unwrap_or_default()
            .as_micros()
            .min(u128::from(u64::MAX)) as u64;
        let chunk = clock
            .push_pcm(&pcm)
            .expect("a fixed-size audio period is sample-aligned");
        if sender
            .send(AudioCaptureEvent::Chunk(TimedAudioChunk {
                observed_start: MonotonicTimestamp::from_micros(observed_us),
                timestamp_accuracy: TimestampAccuracy::Estimated,
                format,
                chunk,
                source_discontinuity: false,
            }))
            .is_err()
        {
            return;
        }
    }
}

fn configure_pcm(device: &str) -> Result<PCM, AudioCaptureError> {
    let pcm = PCM::new(device, Direction::Capture, true)?;
    {
        let hardware = HwParams::any(&pcm)?;
        hardware.set_access(Access::RWInterleaved)?;
        hardware.set_format(Format::S16LE)?;
        hardware.set_channels(u32::from(AUDIO_CHANNELS))?;
        let rate = hardware.set_rate_near(AUDIO_SAMPLE_RATE_HZ, alsa::ValueOr::Nearest)?;
        if rate != AUDIO_SAMPLE_RATE_HZ {
            return Err(AudioCaptureError::UnexpectedRate {
                expected: AUDIO_SAMPLE_RATE_HZ,
                actual: rate,
            });
        }
        let period =
            hardware.set_period_size_near(PERIOD_SAMPLE_FRAMES as i64, alsa::ValueOr::Nearest)?;
        if period != PERIOD_SAMPLE_FRAMES as i64 {
            return Err(AudioCaptureError::UnexpectedPeriod {
                expected: PERIOD_SAMPLE_FRAMES as i64,
                actual: period,
            });
        }
        hardware.set_buffer_size((PERIOD_SAMPLE_FRAMES * 4) as i64)?;
        pcm.hw_params(&hardware)?;
    }
    {
        let software = pcm.sw_params_current()?;
        software.set_avail_min(PERIOD_SAMPLE_FRAMES as i64)?;
        software.set_tstamp_mode(true)?;
        software.set_tstamp_type(TstampType::Monotonic)?;
        pcm.sw_params(&software)?;
    }
    pcm.prepare()?;
    pcm.start()?;
    Ok(pcm)
}

fn run_alsa_capture(
    pcm: PCM,
    monotonic_origin_ns: u64,
    running: Arc<AtomicBool>,
    sender: Sender<AudioCaptureEvent>,
) {
    let format = AudioFormat::pcm_s16le(AUDIO_SAMPLE_RATE_HZ, u32::from(AUDIO_CHANNELS))
        .expect("the camera audio format is valid");
    let io = match pcm.io_i16() {
        Ok(io) => io,
        Err(error) => {
            let _ = sender.send(AudioCaptureEvent::Failed(error.to_string()));
            return;
        }
    };
    let mut clock = AudioClock::new();
    let mut source_discontinuity = false;
    while running.load(Ordering::Acquire) {
        match pcm.wait(Some(100)) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(error) => {
                let _ = sender.send(AudioCaptureEvent::Failed(error.to_string()));
                return;
            }
        }
        let mut samples = vec![0_i16; PERIOD_SAMPLE_FRAMES * usize::from(AUDIO_CHANNELS)];
        let mut frames_read = 0;
        while frames_read < PERIOD_SAMPLE_FRAMES && running.load(Ordering::Acquire) {
            match io.readi(&mut samples[frames_read * usize::from(AUDIO_CHANNELS)..]) {
                Ok(0) => continue,
                Ok(frames) => frames_read += frames,
                Err(error) if error.errno() == libc::EAGAIN => match pcm.wait(Some(100)) {
                    Ok(_) => {}
                    Err(wait_error) => {
                        let _ = sender.send(AudioCaptureEvent::Failed(wait_error.to_string()));
                        return;
                    }
                },
                Err(error) => match pcm.try_recover(error, false) {
                    Ok(()) => {
                        source_discontinuity = true;
                        frames_read = 0;
                    }
                    Err(error) => {
                        let _ = sender.send(AudioCaptureEvent::Failed(error.to_string()));
                        return;
                    }
                },
            }
        }
        if frames_read != PERIOD_SAMPLE_FRAMES {
            continue;
        }

        let status = match pcm.status() {
            Ok(status) => status,
            Err(error) => {
                let _ = sender.send(AudioCaptureEvent::Failed(error.to_string()));
                return;
            }
        };
        let status_ns = timespec_ns(status.get_htstamp());
        let available_frames = status.get_avail().max(0) as u64;
        let first_sample_ns = first_sample_timestamp_ns(
            status_ns,
            available_frames,
            PERIOD_SAMPLE_FRAMES as u64,
            AUDIO_SAMPLE_RATE_HZ,
        );
        let observed_us = first_sample_ns.saturating_sub(monotonic_origin_ns) / 1_000;
        let mut pcm_bytes = Vec::with_capacity(samples.len() * 2);
        for sample in samples {
            pcm_bytes.extend_from_slice(&sample.to_le_bytes());
        }
        let chunk = clock
            .push_pcm(&pcm_bytes)
            .expect("a fixed-size audio period is sample-aligned");
        if sender
            .send(AudioCaptureEvent::Chunk(TimedAudioChunk {
                observed_start: MonotonicTimestamp::from_micros(observed_us),
                timestamp_accuracy: TimestampAccuracy::Hardware,
                format,
                chunk,
                source_discontinuity: std::mem::take(&mut source_discontinuity),
            }))
            .is_err()
        {
            return;
        }
    }
    let _ = pcm.drop();
    let _ = sender.send(AudioCaptureEvent::Closed);
}

fn first_sample_timestamp_ns(
    status_ns: u64,
    available_frames: u64,
    period_frames: u64,
    sample_rate_hz: u32,
) -> u64 {
    let frames_before_hardware = available_frames.saturating_add(period_frames);
    status_ns.saturating_sub(
        frames_before_hardware.saturating_mul(1_000_000_000) / u64::from(sample_rate_hz),
    )
}

fn estimate_monotonic_origin_ns(session_origin: Instant) -> u64 {
    let elapsed_ns = Instant::now()
        .saturating_duration_since(session_origin)
        .as_nanos()
        .min(u128::from(u64::MAX)) as u64;
    monotonic_now_ns().saturating_sub(elapsed_ns)
}

fn monotonic_now_ns() -> u64 {
    let mut timestamp = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `timestamp` points to valid writable storage for clock_gettime.
    let result = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut timestamp) };
    if result != 0 {
        return 0;
    }
    timespec_ns(timestamp)
}

fn timespec_ns(timestamp: libc::timespec) -> u64 {
    u64::try_from(timestamp.tv_sec)
        .unwrap_or(0)
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::try_from(timestamp.tv_nsec).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_emits_complete_timestamped_periods() {
        let input = vec![0_u8; PERIOD_BYTES * 2];
        let (sender, receiver) = mpsc::channel();
        run_capture(
            input.as_slice(),
            Instant::now() - Duration::from_secs(1),
            sender,
        );
        let first = match receiver.recv().unwrap() {
            AudioCaptureEvent::Chunk(chunk) => chunk,
            event => panic!("unexpected event: {event:?}"),
        };
        let second = match receiver.recv().unwrap() {
            AudioCaptureEvent::Chunk(chunk) => chunk,
            event => panic!("unexpected event: {event:?}"),
        };
        assert_eq!(first.chunk.start_sample_frame, 0);
        assert_eq!(second.chunk.start_sample_frame, 640);
        assert_eq!(first.timestamp_accuracy, TimestampAccuracy::Estimated);
        assert!(matches!(
            receiver.recv().unwrap(),
            AudioCaptureEvent::Closed
        ));
    }

    #[test]
    fn derives_period_start_from_status_time_and_unread_frames() {
        assert_eq!(
            first_sample_timestamp_ns(2_000_000_000, 160, 640, 16_000),
            1_950_000_000
        );
    }
}
