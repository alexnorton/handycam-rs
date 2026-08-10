use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::NonNull;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use handycam_core::{
    COMMAND_BLOCK_INDEX, Endpoint, InitOp, PRODUCT_ID, TransportCommand, TransportCommandEncoder,
    VENDOR_ID, VIDEO_ALT, VIDEO_INTERFACE, scan_ack_token,
};
use rusb::ffi;
use rusb::{Context, Device, DeviceHandle, Direction, TransferType, UsbContext};
use thiserror::Error;
use tracing::{debug, warn};

const EP_BOUNDARY: u8 = 0x81;
const EP_VIDEO: u8 = 0x82;
const BOUNDARY_PACKET_SIZE: usize = 8;
const VIDEO_PACKET_SIZE: usize = 768;
const TRANSFERS_PER_ENDPOINT: usize = 32;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(1);
const STATUS_INDEX: u16 = 0x0340;
const STATUS_SIZE: usize = 64;
const STATUS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const STATUS_POLL_TIMEOUT: Duration = Duration::from_millis(500);
const STARTUP_SCAN_TOKENS: [u8; 4] = [0x71, 0x81, 0x91, 0xa1];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InitStrategy {
    /// Reproduce every captured control operation and delay.
    #[default]
    LiteralReplay,
    /// Replace startup read-runs with record-mode `n1` acknowledgement polling.
    ConditionPolling,
    /// Replace startup read-runs with playback-mode `n9` acknowledgement polling.
    PlaybackConditionPolling,
}

impl InitStrategy {
    fn acknowledgement_token(self, command_token: u8) -> Option<u8> {
        match self {
            Self::LiteralReplay => None,
            Self::ConditionPolling => Some(command_token),
            Self::PlaybackConditionPolling => Some(command_token | 0x08),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CameraSelector {
    pub bus: Option<u8>,
    pub ports: Vec<u8>,
}

impl fmt::Display for CameraSelector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.bus {
            None => formatter.write_str("any 054c:00c0 camera"),
            Some(bus) => {
                write!(formatter, "{bus:03}-")?;
                for (index, port) in self.ports.iter().enumerate() {
                    if index != 0 {
                        formatter.write_str(".")?;
                    }
                    write!(formatter, "{port}")?;
                }
                Ok(())
            }
        }
    }
}

impl FromStr for CameraSelector {
    type Err = UsbTransportError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (bus, ports) = value
            .split_once('-')
            .ok_or_else(|| UsbTransportError::InvalidUsbPath(value.to_owned()))?;
        let bus = bus
            .parse::<u8>()
            .map_err(|_| UsbTransportError::InvalidUsbPath(value.to_owned()))?;
        let ports = ports
            .split('.')
            .map(|part| {
                part.parse::<u8>()
                    .map_err(|_| UsbTransportError::InvalidUsbPath(value.to_owned()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if bus == 0 || ports.is_empty() || ports.contains(&0) {
            return Err(UsbTransportError::InvalidUsbPath(value.to_owned()));
        }
        Ok(Self {
            bus: Some(bus),
            ports,
        })
    }
}

#[derive(Clone, Debug)]
pub enum SessionEvent {
    Packet {
        endpoint: Endpoint,
        data: Vec<u8>,
        /// Host monotonic time captured at the start of the libusb callback.
        ///
        /// The capture pipeline correlates this with ALSA's monotonic capture
        /// timestamp without relying on the camera's occasionally frozen
        /// 11-bit video timestamp.
        received_at: Instant,
    },
    PacketError {
        endpoint: Endpoint,
        status: i32,
    },
    TransferFailed {
        endpoint: Endpoint,
        status: i32,
    },
    SubmitFailed {
        endpoint: Endpoint,
        error: i32,
    },
    CallbackPanicked,
}

impl SessionEvent {
    pub fn ends_session(&self) -> bool {
        matches!(
            self,
            Self::TransferFailed { .. } | Self::SubmitFailed { .. } | Self::CallbackPanicked
        )
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SessionStats {
    pub packets: u64,
    pub nonempty_packets: u64,
    pub packet_errors: u64,
    pub bytes: u64,
    pub transfer_errors: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportObservation {
    pub bus: u8,
    pub address: u8,
    pub port_path: Vec<u8>,
    pub command_word: u32,
    pub sequence: u8,
    pub status_before: [u8; STATUS_SIZE],
    pub status_after: [u8; STATUS_SIZE],
}

#[derive(Default)]
struct AtomicSessionStats {
    packets: AtomicU64,
    nonempty_packets: AtomicU64,
    packet_errors: AtomicU64,
    bytes: AtomicU64,
    transfer_errors: AtomicU64,
}

impl AtomicSessionStats {
    fn snapshot(&self) -> SessionStats {
        SessionStats {
            packets: self.packets.load(Ordering::Relaxed),
            nonempty_packets: self.nonempty_packets.load(Ordering::Relaxed),
            packet_errors: self.packet_errors.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            transfer_errors: self.transfer_errors.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Error)]
pub enum UsbTransportError {
    #[error("invalid USB path {0:?}; expected BUS-PORT[.PORT...]")]
    InvalidUsbPath(String),
    #[error("Sony camera 054c:00c0 is not connected")]
    NoCamera,
    #[error("multiple Sony cameras match; select one with --usb-path: {0}")]
    AmbiguousCamera(String),
    #[error("USB operation failed: {0}")]
    Usb(#[from] rusb::Error),
    #[error("interface 0 already has a kernel driver")]
    KernelDriverActive,
    #[error("camera descriptors do not provide interface 0 alt 5 endpoints 0x81/0x82")]
    UnexpectedDescriptors,
    #[error(
        "initialization control read index {index:#06x} returned {actual} bytes, expected {expected}"
    )]
    ShortControlRead {
        index: u16,
        expected: usize,
        actual: usize,
    },
    #[error(
        "initialization control write index {index:#06x} wrote {actual} bytes, expected {expected}"
    )]
    ShortControlWrite {
        index: u16,
        expected: usize,
        actual: usize,
    },
    #[error("could not allocate an isochronous libusb transfer")]
    TransferAllocation,
    #[error("could not submit an isochronous transfer: libusb error {0}")]
    TransferSubmission(i32),
    #[error("libusb event handling failed: {0}")]
    EventHandling(rusb::Error),
    #[error(
        "startup command token {token:#04x} was not acknowledged within 500 ms \
         (last status byte was {last_status:#04x})"
    )]
    StatusPollTimeout { token: u8, last_status: u8 },
}

struct TransferSlot {
    transfer: NonNull<ffi::libusb_transfer>,
    buffer: Box<[u8]>,
    endpoint: Endpoint,
    submitted: AtomicBool,
    running: Arc<AtomicBool>,
    events: mpsc::Sender<SessionEvent>,
    stats: Arc<AtomicSessionStats>,
}

impl TransferSlot {
    fn endpoint_address(&self) -> u8 {
        match self.endpoint {
            Endpoint::Boundary => EP_BOUNDARY,
            Endpoint::Video => EP_VIDEO,
        }
    }
}

/// A claimed and initialized camera session.
pub struct UsbSession {
    context: Context,
    handle: DeviceHandle<Context>,
    // Each callback's user_data points at a slot, so the allocation must not
    // move when this Vec grows.
    #[allow(clippy::vec_box)]
    slots: Vec<Box<TransferSlot>>,
    events: mpsc::Receiver<SessionEvent>,
    running: Arc<AtomicBool>,
    stats: Arc<AtomicSessionStats>,
    stopped: bool,
    bus: u8,
    address: u8,
    port_path: Vec<u8>,
}

impl UsbSession {
    pub fn open(selector: &CameraSelector, plan: &[InitOp]) -> Result<Self, UsbTransportError> {
        Self::open_with_init_strategy(selector, plan, InitStrategy::LiteralReplay)
    }

    pub fn open_with_init_strategy(
        selector: &CameraSelector,
        plan: &[InitOp],
        init_strategy: InitStrategy,
    ) -> Result<Self, UsbTransportError> {
        let context = Context::new()?;
        let device = select_device(&context, selector)?;
        validate_descriptors(&device)?;
        let bus = device.bus_number();
        let address = device.address();
        let port_path = device.port_numbers()?;
        let handle = device.open()?;

        match handle.kernel_driver_active(VIDEO_INTERFACE) {
            Ok(true) => return Err(UsbTransportError::KernelDriverActive),
            Ok(false) | Err(rusb::Error::NotSupported) => {}
            Err(error) => return Err(error.into()),
        }
        handle.claim_interface(VIDEO_INTERFACE)?;
        if let Err(error) = replay_initialization(&handle, plan, init_strategy) {
            let _ = handle.set_alternate_setting(VIDEO_INTERFACE, 0);
            let _ = handle.release_interface(VIDEO_INTERFACE);
            return Err(error);
        }

        let running = Arc::new(AtomicBool::new(true));
        let stats = Arc::new(AtomicSessionStats::default());
        let (sender, receiver) = mpsc::channel();
        let mut session = Self {
            context,
            handle,
            slots: Vec::with_capacity(TRANSFERS_PER_ENDPOINT * 2),
            events: receiver,
            running,
            stats,
            stopped: false,
            bus,
            address,
            port_path,
        };
        session.allocate_and_submit(sender)?;
        Ok(session)
    }

    pub fn bus(&self) -> u8 {
        self.bus
    }

    pub fn address(&self) -> u8 {
        self.address
    }

    pub fn port_path(&self) -> &[u8] {
        &self.port_path
    }

    pub fn stats(&self) -> SessionStats {
        self.stats.snapshot()
    }

    /// Drive libusb callbacks for at most `timeout`, then drain all events
    /// generated by those callbacks.
    pub fn poll(&mut self, timeout: Duration) -> Result<Vec<SessionEvent>, UsbTransportError> {
        self.context
            .handle_events(Some(timeout))
            .map_err(UsbTransportError::EventHandling)?;
        Ok(self.events.try_iter().collect())
    }

    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.running.store(false, Ordering::Release);

        for slot in &self.slots {
            if slot.submitted.load(Ordering::Acquire) {
                // SAFETY: The transfer remains allocated until every cancelled
                // callback has run below.
                let result = unsafe { ffi::libusb_cancel_transfer(slot.transfer.as_ptr()) };
                if result != 0 && result != ffi::constants::LIBUSB_ERROR_NOT_FOUND {
                    warn!(
                        endpoint = format_args!("{:#04x}", slot.endpoint_address()),
                        error = result,
                        "could not cancel libusb transfer"
                    );
                }
            }
        }

        while self
            .slots
            .iter()
            .any(|slot| slot.submitted.load(Ordering::Acquire))
        {
            if let Err(error) = self.context.handle_events(Some(Duration::from_millis(100))) {
                warn!(%error, "libusb error while reaping cancelled transfers");
            }
        }

        if let Err(error) = self.handle.set_alternate_setting(VIDEO_INTERFACE, 0) {
            debug!(%error, "could not restore interface 0 alt 0");
        }
        if let Err(error) = self.handle.release_interface(VIDEO_INTERFACE) {
            debug!(%error, "could not release interface 0");
        }

        for slot in self.slots.drain(..) {
            // SAFETY: The transfer is no longer submitted and its callback can
            // no longer run. libusb owns no buffer memory.
            unsafe { ffi::libusb_free_transfer(slot.transfer.as_ptr()) };
        }
    }

    fn allocate_and_submit(
        &mut self,
        sender: mpsc::Sender<SessionEvent>,
    ) -> Result<(), UsbTransportError> {
        for index in 0..(TRANSFERS_PER_ENDPOINT * 2) {
            let endpoint = if index % 2 == 0 {
                Endpoint::Boundary
            } else {
                Endpoint::Video
            };
            let packet_size = match endpoint {
                Endpoint::Boundary => BOUNDARY_PACKET_SIZE,
                Endpoint::Video => VIDEO_PACKET_SIZE,
            };
            // SAFETY: libusb returns either null or a transfer allocation
            // containing one trailing ISO descriptor.
            let transfer = NonNull::new(unsafe { ffi::libusb_alloc_transfer(1) })
                .ok_or(UsbTransportError::TransferAllocation)?;
            let slot = Box::new(TransferSlot {
                transfer,
                buffer: vec![0; packet_size].into_boxed_slice(),
                endpoint,
                submitted: AtomicBool::new(false),
                running: Arc::clone(&self.running),
                events: sender.clone(),
                stats: Arc::clone(&self.stats),
            });
            self.slots.push(slot);
            let slot = self.slots.last_mut().expect("slot was just pushed");
            let slot_pointer: *mut TransferSlot = &mut **slot;

            // SAFETY: The boxed slot and its fixed-size boxed buffer have stable
            // addresses through cancellation/reaping in `stop`.
            unsafe {
                ffi::libusb_fill_iso_transfer(
                    transfer.as_ptr(),
                    self.handle.as_raw(),
                    slot.endpoint_address(),
                    slot.buffer.as_mut_ptr(),
                    packet_size as i32,
                    1,
                    transfer_callback,
                    slot_pointer.cast(),
                    1000,
                );
                ffi::libusb_set_iso_packet_lengths(transfer.as_ptr(), packet_size as u32);
                let result = ffi::libusb_submit_transfer(transfer.as_ptr());
                if result != 0 {
                    return Err(UsbTransportError::TransferSubmission(result));
                }
            }
            slot.submitted.store(true, Ordering::Release);
        }
        Ok(())
    }
}

impl Drop for UsbSession {
    fn drop(&mut self) {
        self.stop();
    }
}

extern "system" fn transfer_callback(transfer: *mut ffi::libusb_transfer) {
    // Never permit Rust unwinding to cross libusb's C callback boundary.
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: libusb invokes this callback with the submitted transfer.
        // user_data points to the boxed TransferSlot retained by UsbSession.
        unsafe { handle_transfer_callback(transfer) };
    }));
    if result.is_err() && !transfer.is_null() {
        // SAFETY: The same user_data guarantee applies while inside callback.
        let slot = unsafe { ((*transfer).user_data as *mut TransferSlot).as_ref() };
        if let Some(slot) = slot {
            slot.running.store(false, Ordering::Release);
            slot.submitted.store(false, Ordering::Release);
            let _ = slot.events.send(SessionEvent::CallbackPanicked);
        }
    }
}

unsafe fn handle_transfer_callback(transfer: *mut ffi::libusb_transfer) {
    // SAFETY: Preconditions are established by `transfer_callback`.
    let slot = unsafe { &*((*transfer).user_data as *mut TransferSlot) };
    let transfer_status = unsafe { (*transfer).status };
    let received_at = Instant::now();

    if transfer_status == ffi::constants::LIBUSB_TRANSFER_COMPLETED {
        let descriptor = unsafe { &*(*transfer).iso_packet_desc.as_ptr() };
        slot.stats.packets.fetch_add(1, Ordering::Relaxed);
        if descriptor.status == ffi::constants::LIBUSB_TRANSFER_COMPLETED {
            let length = descriptor.actual_length;
            if length as usize > slot.buffer.len() {
                slot.stats.packet_errors.fetch_add(1, Ordering::Relaxed);
                let _ = slot.events.send(SessionEvent::PacketError {
                    endpoint: slot.endpoint,
                    status: ffi::constants::LIBUSB_TRANSFER_OVERFLOW,
                });
            } else if length > 0 {
                let length = length as usize;
                slot.stats.nonempty_packets.fetch_add(1, Ordering::Relaxed);
                slot.stats.bytes.fetch_add(length as u64, Ordering::Relaxed);
                let data = slot.buffer[..length].to_vec();
                let _ = slot.events.send(SessionEvent::Packet {
                    endpoint: slot.endpoint,
                    data,
                    received_at,
                });
            }
        } else {
            slot.stats.packet_errors.fetch_add(1, Ordering::Relaxed);
            let _ = slot.events.send(SessionEvent::PacketError {
                endpoint: slot.endpoint,
                status: descriptor.status,
            });
        }
    } else if transfer_status != ffi::constants::LIBUSB_TRANSFER_CANCELLED {
        slot.stats.transfer_errors.fetch_add(1, Ordering::Relaxed);
        slot.running.store(false, Ordering::Release);
        let _ = slot.events.send(SessionEvent::TransferFailed {
            endpoint: slot.endpoint,
            status: transfer_status,
        });
    }

    if slot.running.load(Ordering::Acquire) {
        unsafe { (*transfer).actual_length = 0 };
        let result = unsafe { ffi::libusb_submit_transfer(transfer) };
        if result == 0 {
            return;
        }
        slot.running.store(false, Ordering::Release);
        let _ = slot.events.send(SessionEvent::SubmitFailed {
            endpoint: slot.endpoint,
            error: result,
        });
    }
    slot.submitted.store(false, Ordering::Release);
}

fn select_device(
    context: &Context,
    selector: &CameraSelector,
) -> Result<Device<Context>, UsbTransportError> {
    let mut matches = Vec::new();
    for device in context.devices()?.iter() {
        let descriptor = match device.device_descriptor() {
            Ok(descriptor) => descriptor,
            Err(_) => continue,
        };
        if descriptor.vendor_id() != VENDOR_ID || descriptor.product_id() != PRODUCT_ID {
            continue;
        }
        if let Some(bus) = selector.bus {
            if device.bus_number() != bus || device.port_numbers()? != selector.ports {
                continue;
            }
        }
        matches.push(device);
    }
    match matches.len() {
        0 => Err(UsbTransportError::NoCamera),
        1 => Ok(matches.remove(0)),
        _ => {
            let paths = matches
                .iter()
                .map(|device| {
                    let ports = device
                        .port_numbers()
                        .unwrap_or_default()
                        .iter()
                        .map(u8::to_string)
                        .collect::<Vec<_>>()
                        .join(".");
                    format!("{:03}-{ports}", device.bus_number())
                })
                .collect::<Vec<_>>()
                .join(", ");
            Err(UsbTransportError::AmbiguousCamera(paths))
        }
    }
}

fn read_status_block(
    handle: &DeviceHandle<Context>,
) -> Result<[u8; STATUS_SIZE], UsbTransportError> {
    let mut status = [0_u8; STATUS_SIZE];
    let actual = handle.read_control(0xc0, 0x88, 0, STATUS_INDEX, &mut status, CONTROL_TIMEOUT)?;
    if actual != status.len() {
        return Err(UsbTransportError::ShortControlRead {
            index: STATUS_INDEX,
            expected: status.len(),
            actual,
        });
    }
    Ok(status)
}

/// Read the camera's transport/status mailbox without changing device state.
pub fn read_camera_status(
    selector: &CameraSelector,
) -> Result<[u8; STATUS_SIZE], UsbTransportError> {
    let context = Context::new()?;
    let device = select_device(&context, selector)?;
    let handle = device.open()?;
    read_status_block(&handle)
}

/// Send exactly one recovered tape-transport command and observe status.
///
/// This deliberately avoids record-mode initialization and is intended for
/// controlled playback-mode validation. When `current_sequence` is absent,
/// the current counter is recovered from the high nibble of status byte 0.
/// The Sony encoder increments it before use.
pub fn send_transport_command(
    selector: &CameraSelector,
    command: TransportCommand,
    current_sequence: Option<u8>,
    settle: Duration,
) -> Result<TransportObservation, UsbTransportError> {
    let context = Context::new()?;
    let device = select_device(&context, selector)?;
    let bus = device.bus_number();
    let address = device.address();
    let port_path = device.port_numbers()?;
    let handle = device.open()?;
    let status_before = read_status_block(&handle)?;

    let current_sequence = current_sequence.unwrap_or(status_before[0] >> 4);
    let mut encoder = TransportCommandEncoder::with_sequence(current_sequence);
    let command_word = encoder.encode(command);
    let block = handycam_core::command_block(command_word);
    let actual =
        handle.write_control(0x40, 0x88, 0, COMMAND_BLOCK_INDEX, &block, CONTROL_TIMEOUT)?;
    if actual != block.len() {
        return Err(UsbTransportError::ShortControlWrite {
            index: COMMAND_BLOCK_INDEX,
            expected: block.len(),
            actual,
        });
    }
    std::thread::sleep(settle);
    let status_after = read_status_block(&handle)?;

    Ok(TransportObservation {
        bus,
        address,
        port_path,
        command_word,
        sequence: encoder.sequence(),
        status_before,
        status_after,
    })
}

fn validate_descriptors(device: &Device<Context>) -> Result<(), UsbTransportError> {
    let configuration = device.active_config_descriptor()?;
    for interface in configuration.interfaces() {
        if interface.number() != VIDEO_INTERFACE {
            continue;
        }
        for descriptor in interface.descriptors() {
            if descriptor.setting_number() != VIDEO_ALT {
                continue;
            }
            let mut boundary = false;
            let mut video = false;
            for endpoint in descriptor.endpoint_descriptors() {
                if endpoint.direction() != Direction::In
                    || endpoint.transfer_type() != TransferType::Isochronous
                {
                    continue;
                }
                match endpoint.address() {
                    EP_BOUNDARY if endpoint.max_packet_size() >= 8 => boundary = true,
                    EP_VIDEO if endpoint.max_packet_size() >= 768 => video = true,
                    _ => {}
                }
            }
            if boundary && video {
                return Ok(());
            }
        }
    }
    Err(UsbTransportError::UnexpectedDescriptors)
}

fn replay_initialization(
    handle: &DeviceHandle<Context>,
    plan: &[InitOp],
    strategy: InitStrategy,
) -> Result<(), UsbTransportError> {
    let mut position = 0;
    while position < plan.len() {
        let operation = &plan[position];
        std::thread::sleep(Duration::from_micros(operation.delay_us()));
        match operation {
            InitOp::ControlIn {
                request_type,
                request,
                value,
                index,
                length,
                ..
            } => {
                let mut data = vec![0; usize::from(*length)];
                let actual = handle.read_control(
                    *request_type,
                    *request,
                    *value,
                    *index,
                    &mut data,
                    CONTROL_TIMEOUT,
                )?;
                if actual != data.len() {
                    return Err(UsbTransportError::ShortControlRead {
                        index: *index,
                        expected: data.len(),
                        actual,
                    });
                }
            }
            InitOp::ControlOut {
                request_type,
                request,
                value,
                index,
                data,
                ..
            } => {
                let actual = handle.write_control(
                    *request_type,
                    *request,
                    *value,
                    *index,
                    data,
                    CONTROL_TIMEOUT,
                )?;
                if actual != data.len() {
                    return Err(UsbTransportError::ShortControlWrite {
                        index: *index,
                        expected: data.len(),
                        actual,
                    });
                }
                if let Some(command_token) = startup_scan_token(operation)
                    && let Some(acknowledgement_token) =
                        strategy.acknowledgement_token(command_token)
                {
                    let next = end_of_status_poll_run(plan, position + 1);
                    if next > position + 1 {
                        let attempts = poll_status_acknowledgement(handle, acknowledgement_token)?;
                        debug!(
                            command_token = format_args!("{command_token:#04x}"),
                            acknowledgement_token = format_args!("{acknowledgement_token:#04x}"),
                            attempts,
                            skipped_literal_reads = next - position - 1,
                            "startup command acknowledged"
                        );
                        position = next;
                        continue;
                    }
                }
            }
            InitOp::SetInterface {
                interface,
                alternate,
                ..
            } => handle.set_alternate_setting(*interface, *alternate)?,
        }
        position += 1;
    }
    Ok(())
}

fn startup_scan_token(operation: &InitOp) -> Option<u8> {
    let InitOp::ControlOut { index, data, .. } = operation else {
        return None;
    };
    if *index != COMMAND_BLOCK_INDEX {
        return None;
    }
    scan_ack_token(data).filter(|token| STARTUP_SCAN_TOKENS.contains(token))
}

fn is_status_poll(operation: &InitOp) -> bool {
    matches!(
        operation,
        InitOp::ControlIn {
            request_type: 0xc0,
            request: 0x88,
            value: 0,
            index: STATUS_INDEX,
            length: 64,
            ..
        }
    )
}

fn end_of_status_poll_run(plan: &[InitOp], start: usize) -> usize {
    let mut position = start;
    while plan.get(position).is_some_and(is_status_poll) {
        position += 1;
    }
    position
}

fn poll_status_acknowledgement(
    handle: &DeviceHandle<Context>,
    token: u8,
) -> Result<usize, UsbTransportError> {
    let deadline = Instant::now() + STATUS_POLL_TIMEOUT;
    let mut attempts = 0;
    loop {
        std::thread::sleep(STATUS_POLL_INTERVAL);
        let status = read_status_block(handle)?;
        attempts += 1;
        let last_status = status[0];
        if last_status == token {
            return Ok(attempts);
        }
        if Instant::now() >= deadline {
            return Err(UsbTransportError::StatusPollTimeout { token, last_status });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_physical_port_path() {
        let selector: CameraSelector = "001-2.3".parse().unwrap();
        assert_eq!(selector.bus, Some(1));
        assert_eq!(selector.ports, [2, 3]);
        assert_eq!(selector.to_string(), "001-2.3");
    }

    #[test]
    fn rejects_address_instead_of_port_path() {
        assert!("001:004".parse::<CameraSelector>().is_err());
    }

    #[test]
    fn identifies_only_the_four_startup_scan_groups() {
        let plan = handycam_core::record_mode_init_plan().unwrap();
        let groups = plan
            .iter()
            .enumerate()
            .filter_map(|(position, operation)| {
                startup_scan_token(operation).map(|token| {
                    (
                        token,
                        end_of_status_poll_run(&plan, position + 1) - position - 1,
                    )
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(groups, [(0x71, 16), (0x81, 14), (0x91, 16), (0xa1, 11)]);
    }

    #[test]
    fn initialization_strategies_select_mode_specific_acknowledgements() {
        assert_eq!(
            InitStrategy::LiteralReplay.acknowledgement_token(0x71),
            None
        );
        assert_eq!(
            InitStrategy::ConditionPolling.acknowledgement_token(0x71),
            Some(0x71)
        );
        assert_eq!(
            InitStrategy::PlaybackConditionPolling.acknowledgement_token(0x71),
            Some(0x79)
        );
    }
}
