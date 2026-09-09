use crate::{Error, Result};
mod progress;
use spandsp::{
    fax::FaxState,
    spandsp_sys as sys,
    t30::{T30ModemSupport, T30State},
    t38_core::{T38Core, T38DataRateManagement, T38Version},
    t38_terminal::T38Terminal,
};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    ffi::{CStr, CString, c_void},
    path::Path,
};

pub const FRAME_SAMPLES: usize = 160;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PageProgress {
    /// Zero-based page index. These rows are transmitted, not acknowledged.
    pub page: u32,
    pub rows: u32,
    pub total_rows: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferStats {
    pub sent_pages: u32,
    pub received_pages: u32,
    /// Decoded rows in the current receive image; these are not confirmed pages.
    pub received_rows: u32,
    pub bit_rate: u32,
    pub ecm: bool,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("T.30 {code}: {reason}")]
pub struct FaxFailure {
    pub code: i32,
    pub reason: String,
    pub stats: TransferStats,
}

#[derive(Debug, Clone)]
pub enum FaxEvent {
    Stage(FaxStage),
    Negotiated,
    PageAcknowledged(TransferStats),
    PageProgress(PageProgress),
    /// Actual outgoing UDPTL bytes, including redundancy and retransmissions.
    T38Activity {
        transmitted_bytes: u64,
    },
    Completed(std::result::Result<TransferStats, FaxFailure>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum FaxStage {
    Connecting,
    Securing,
    Registering,
    Calling,
    Ringing,
    Connected,
    OfferingT38,
    FallingBack,
    Negotiating,
    SendingT38,
    SendingG711,
    Confirming,
}

impl std::fmt::Display for FaxStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Connecting => "Setting up connection",
            Self::Securing => "Securing connection with TLS",
            Self::Registering => "Registering with SIP server",
            Self::Calling => "Calling destination",
            Self::Ringing => "Waiting for answer",
            Self::Connected => "Call answered · setting up media",
            Self::OfferingT38 => "Negotiating T.38",
            Self::FallingBack => "Switching to G.711",
            Self::Negotiating => "Negotiating fax capabilities",
            Self::SendingT38 => "Sending over T.38",
            Self::SendingG711 => "Sending over G.711",
            Self::Confirming => "Confirming delivery",
        })
    }
}

#[derive(Default)]
struct Callbacks {
    negotiated: Cell<bool>,
    page: Cell<bool>,
    completion: Cell<Option<i32>>,
    packets: RefCell<VecDeque<IfpPacket>>,
    progress: RefCell<progress::Progress>,
    t30: Cell<*mut sys::t30_state_t>,
    monitor: RefCell<Option<T38Core<'static>>>,
    monitor_sequence: Cell<u16>,
}

impl Callbacks {
    fn pointer(&self) -> *mut c_void {
        std::ptr::from_ref(self).cast_mut().cast()
    }

    fn events(&self, t30: &T30State) -> Vec<FaxEvent> {
        let mut events = Vec::new();
        if let Some(progress) = self.progress.borrow_mut().update() {
            events.push(FaxEvent::PageProgress(progress));
        }
        if self.negotiated.replace(false) {
            events.push(FaxEvent::Negotiated);
        }
        if self.page.replace(false) {
            events.push(FaxEvent::PageAcknowledged(statistics(t30)));
        }
        if let Some(code) = self.completion.take() {
            let stats = statistics(t30);
            events.push(FaxEvent::Completed(match code {
                0 => Ok(stats),
                code => {
                    // SpanDSP returns a process-lifetime string for every completion code.
                    let reason = unsafe { CStr::from_ptr(sys::t30_completion_code_to_str(code)) }
                        .to_string_lossy()
                        .into_owned();
                    Err(FaxFailure {
                        code,
                        reason,
                        stats,
                    })
                }
            }));
        }
        events
    }
}

/// Native state is dropped before its callback storage.
pub struct AudioFax {
    state: FaxState,
    callbacks: Box<Callbacks>,
}

impl AudioFax {
    pub fn transmitter(file: &Path, station_id: &str) -> Result<Self> {
        Self::new(file, station_id, true)
    }
    pub fn receiver(file: &Path, station_id: &str) -> Result<Self> {
        Self::new(file, station_id, false)
    }

    pub fn receiver_with_ecm(file: &Path, station_id: &str, ecm: bool) -> Result<Self> {
        let receiver = Self::receiver(file, station_id)?;
        receiver.state.get_t30_state()?.set_ecm_capability(ecm)?;
        Ok(receiver)
    }

    fn new(file: &Path, station_id: &str, transmitting: bool) -> Result<Self> {
        let state = if transmitting {
            FaxState::new(true)?
        } else {
            FaxState::new_receiver(spandsp::ReceiveRecovery::PreserveDecodedRows)?
        };
        unsafe {
            crate::logging::spandsp(sys::fax_get_logging_state(state.as_ptr()));
        }
        let callbacks = Box::<Callbacks>::default();
        configure(
            &state.get_t30_state()?,
            &callbacks,
            file,
            station_id,
            transmitting,
        )?;
        state.set_transmit_on_idle(true);
        Ok(Self { state, callbacks })
    }

    pub fn receive(&mut self, samples: &mut [i16; FRAME_SAMPLES]) {
        self.state.rx(samples);
    }

    /// Flush recoverable rows and close the TIFF before decoding it. Idempotent.
    pub fn finalize_receive(&mut self) -> crate::ReceiveReport {
        self.state.finalize_receive().into()
    }

    pub fn transmit(&mut self) -> [i16; FRAME_SAMPLES] {
        let mut samples = [0; FRAME_SAMPLES];
        self.state.tx(&mut samples);
        samples
    }

    pub fn events(&mut self) -> Result<Vec<FaxEvent>> {
        Ok(self.callbacks.events(&self.state.get_t30_state()?))
    }
    pub fn statistics(&self) -> Result<TransferStats> {
        Ok(statistics(&self.state.get_t30_state()?))
    }

    pub(crate) fn end_call(&mut self) -> Result<Vec<FaxEvent>> {
        let t30 = self.state.get_t30_state()?;
        // SpanDSP preserves the negotiated result during final disconnect/pause,
        // and reports CALLDROPPED when the protocol was interrupted earlier.
        unsafe { sys::t30_terminate(t30.as_ptr()) };
        Ok(self.callbacks.events(&t30))
    }
}

#[derive(Debug, Clone)]
pub struct IfpPacket {
    pub payload: Vec<u8>,
    /// Repetitions must reuse the same UDPTL sequence number.
    pub repetitions: u32,
}

pub struct PacketFax {
    state: T38Terminal,
    callbacks: Box<Callbacks>,
}

impl PacketFax {
    pub(crate) fn limit_bit_rate(&mut self, bit_rate: u32) -> Result<()> {
        let modems = match bit_rate {
            14400.. => T30ModemSupport::default(),
            9600.. => T30ModemSupport::V27TER | T30ModemSupport::V29,
            _ => T30ModemSupport::V27TER,
        };
        self.state.get_t30_state()?.set_supported_modems(modems)?;
        Ok(())
    }
    pub fn transmitter(file: &Path, station_id: &str) -> Result<Self> {
        Self::new(file, station_id, true)
    }
    pub fn receiver(file: &Path, station_id: &str) -> Result<Self> {
        Self::new(file, station_id, false)
    }

    pub fn receiver_with_ecm(file: &Path, station_id: &str, ecm: bool) -> Result<Self> {
        let receiver = Self::receiver(file, station_id)?;
        receiver.state.get_t30_state()?.set_ecm_capability(ecm)?;
        Ok(receiver)
    }

    fn new(file: &Path, station_id: &str, transmitting: bool) -> Result<Self> {
        let callbacks = Box::<Callbacks>::default();
        if transmitting {
            let monitor = unsafe {
                T38Core::new_raw(
                    Some(monitor_indicator),
                    Some(monitor_data),
                    Some(monitor_missing),
                    callbacks.pointer(),
                    None,
                    std::ptr::null_mut(),
                )
            }?;
            monitor.set_t38_version(T38Version::V0);
            *callbacks.monitor.borrow_mut() = Some(monitor);
        }
        // The boxed callback target never moves and is dropped after the terminal.
        let state = unsafe {
            if transmitting {
                T38Terminal::new_raw(true, Some(tx_packet), callbacks.pointer())
            } else {
                T38Terminal::new_receiver_raw(
                    spandsp::ReceiveRecovery::PreserveDecodedRows,
                    Some(tx_packet),
                    callbacks.pointer(),
                )
            }
        }?;
        unsafe {
            crate::logging::spandsp(sys::t38_terminal_get_logging_state(state.as_ptr()));
        }
        configure(
            &state.get_t30_state()?,
            &callbacks,
            file,
            station_id,
            transmitting,
        )?;
        let core = state.get_t38_core_state()?;
        unsafe {
            crate::logging::spandsp(sys::t38_core_get_logging_state(core.as_ptr()));
        }
        core.set_t38_version(T38Version::V0);
        core.set_data_rate_management_method(T38DataRateManagement::TransferredTcf);
        drop(core);
        Ok(Self { state, callbacks })
    }

    pub fn receive(&mut self, sequence: u16, payload: &[u8]) -> Result<()> {
        if payload.len() > u16::MAX as usize {
            return Err(Error::Invalid("IFP payload exceeds a UDP datagram".into()));
        }
        self.state
            .get_t38_core_state()?
            .rx_ifp_packet(payload, sequence)?;
        Ok(())
    }

    /// Advances the terminal by one 20 ms media interval.
    pub fn tick(&mut self) {
        self.state.send_timeout(FRAME_SAMPLES as i32);
    }
    /// Flush recoverable rows and close the TIFF before decoding it. Idempotent.
    pub fn finalize_receive(&mut self) -> crate::ReceiveReport {
        self.state.finalize_receive().into()
    }
    pub fn packets(&mut self) -> Vec<IfpPacket> {
        self.callbacks.packets.borrow_mut().drain(..).collect()
    }
    pub fn events(&mut self) -> Result<Vec<FaxEvent>> {
        Ok(self.callbacks.events(&self.state.get_t30_state()?))
    }
    pub fn statistics(&self) -> Result<TransferStats> {
        Ok(statistics(&self.state.get_t30_state()?))
    }

    pub(crate) fn end_call(&mut self) -> Result<Vec<FaxEvent>> {
        let t30 = self.state.get_t30_state()?;
        unsafe { sys::t30_terminate(t30.as_ptr()) };
        Ok(self.callbacks.events(&t30))
    }
}

fn configure(
    t30: &T30State,
    callbacks: &Callbacks,
    file: &Path,
    station: &str,
    transmitting: bool,
) -> Result<()> {
    let path = file
        .to_str()
        .ok_or_else(|| Error::Invalid("SpanDSP requires a Unicode spool path".into()))?;
    if station.len() > 20 || !station.is_ascii() || station.chars().any(char::is_control) {
        return Err(Error::Invalid(
            "Station ID must be at most 20 printable ASCII characters".into(),
        ));
    }
    let station = CString::new(station).map_err(|e| Error::Invalid(e.to_string()))?;
    t30.set_supported_modems(T30ModemSupport::default())?;
    t30.set_ecm_capability(true)?;
    t30.set_supported_compressions(
        (sys::t4_image_compression_t_T4_COMPRESSION_T4_1D
            | sys::t4_image_compression_t_T4_COMPRESSION_T4_2D
            | sys::t4_image_compression_t_T4_COMPRESSION_T6) as i32,
    )?;
    match transmitting {
        true => t30.set_tx_file(path, 0, -1)?,
        false => {
            // Group 4 output can be decoded by the engine's Rust TIFF reader.
            let status = unsafe {
                sys::t30_set_supported_output_compressions(
                    t30.as_ptr(),
                    sys::t4_image_compression_t_T4_COMPRESSION_T6 as i32,
                )
            };
            if status != 0 {
                return Err(Error::Invalid(
                    "SpanDSP rejected TIFF output compression".into(),
                ));
            }
            t30.set_rx_file(path, -1)?;
        }
    }
    // Borrowed native handles never escape this configuration call.
    unsafe {
        crate::logging::spandsp(sys::t30_get_logging_state(t30.as_ptr()));
        if sys::t30_set_tx_ident(t30.as_ptr(), station.as_ptr()) != 0 {
            return Err(Error::Invalid("SpanDSP rejected the station ID".into()));
        }
        t30.set_phase_b_handler_raw(Some(phase_b), callbacks.pointer());
        t30.set_phase_d_handler_raw(Some(phase_d), callbacks.pointer());
        t30.set_phase_e_handler_raw(Some(phase_e), callbacks.pointer());
        if transmitting {
            callbacks.t30.set(t30.as_ptr());
            sys::t30_set_real_time_frame_handler(
                t30.as_ptr(),
                Some(transmit_frame),
                callbacks.pointer(),
            );
        }
    }
    Ok(())
}

unsafe extern "C" fn transmit_frame(data: *mut c_void, incoming: bool, bytes: *const u8, len: i32) {
    if bytes.is_null() || len < 3 {
        return;
    }
    let callbacks = unsafe { &*data.cast::<Callbacks>() };
    let mut stats = sys::t30_stats_t::default();
    unsafe { sys::t30_get_transfer_statistics(callbacks.t30.get(), &mut stats) };
    callbacks
        .progress
        .borrow_mut()
        .frame(stats, incoming, unsafe {
            std::slice::from_raw_parts(bytes, len as usize)
        });
}

fn statistics(t30: &T30State) -> TransferStats {
    let stats = t30.get_transfer_statistics();
    TransferStats {
        sent_pages: stats.pages_tx.max(0) as u32,
        received_pages: stats.pages_rx.max(0) as u32,
        received_rows: stats.length.max(0) as u32,
        bit_rate: stats.bit_rate.max(0) as u32,
        ecm: stats.error_correcting_mode != 0,
    }
}

unsafe extern "C" fn phase_b(data: *mut c_void, result: i32) -> i32 {
    tracing::debug!(target: "faxe_native::spandsp", result, "T.30 phase B");
    // All callbacks run synchronously on the media session's thread.
    unsafe { &*data.cast::<Callbacks>() }.negotiated.set(true);
    0
}

unsafe extern "C" fn phase_d(data: *mut c_void, result: i32) -> i32 {
    tracing::debug!(target: "faxe_native::spandsp", result, "T.30 phase D");
    unsafe { &*data.cast::<Callbacks>() }.page.set(true);
    0
}

unsafe extern "C" fn phase_e(data: *mut c_void, result: i32) {
    tracing::debug!(target: "faxe_native::spandsp", result, "T.30 phase E");
    unsafe { &*data.cast::<Callbacks>() }
        .completion
        .set(Some(result));
}

unsafe extern "C" fn tx_packet(
    _core: *mut sys::t38_core_state_t,
    data: *mut c_void,
    bytes: *const u8,
    length: i32,
    repetitions: i32,
) -> i32 {
    if bytes.is_null() || length <= 0 || repetitions <= 0 {
        return 0;
    }
    let callbacks = unsafe { &*data.cast::<Callbacks>() };
    let Ok(mut packets) = callbacks.packets.try_borrow_mut() else {
        return -1;
    };
    if packets.len() >= 1024 || length > u16::MAX as i32 {
        return -1;
    }
    // SpanDSP's buffer is valid only during this callback.
    let payload = unsafe { std::slice::from_raw_parts(bytes, length as usize) }.to_vec();
    packets.push_back(IfpPacket {
        payload,
        repetitions: repetitions as u32,
    });
    drop(packets);
    if let Some(monitor) = callbacks.monitor.borrow_mut().as_mut() {
        let sequence = callbacks.monitor_sequence.get();
        callbacks.monitor_sequence.set(sequence.wrapping_add(1));
        let payload = unsafe { std::slice::from_raw_parts(bytes, length as usize) };
        let _ = monitor.rx_ifp_packet(payload, sequence);
    }
    0
}

unsafe extern "C" fn monitor_indicator(
    _: *mut sys::t38_core_state_t,
    _: *mut c_void,
    _: i32,
) -> i32 {
    0
}
unsafe extern "C" fn monitor_missing(
    _: *mut sys::t38_core_state_t,
    _: *mut c_void,
    _: i32,
    _: i32,
) -> i32 {
    0
}
unsafe extern "C" fn monitor_data(
    _: *mut sys::t38_core_state_t,
    data: *mut c_void,
    _: i32,
    field: i32,
    bytes: *const u8,
    len: i32,
) -> i32 {
    if (field != sys::t38_field_types_e::T38_FIELD_T4_NON_ECM_DATA as i32
        && field != sys::t38_field_types_e::T38_FIELD_T4_NON_ECM_SIG_END as i32)
        || bytes.is_null()
        || len <= 0
    {
        return 0;
    }
    let callbacks = unsafe { &*data.cast::<Callbacks>() };
    let mut stats = sys::t30_stats_t::default();
    unsafe { sys::t30_get_transfer_statistics(callbacks.t30.get(), &mut stats) };
    // T.38 carries non-ECM octets MSB first; the image codec consumes LSB first.
    let bytes: Vec<u8> = unsafe { std::slice::from_raw_parts(bytes, len as usize) }
        .iter()
        .map(|byte| byte.reverse_bits())
        .collect();
    callbacks.progress.borrow_mut().non_ecm_data(stats, &bytes);
    0
}
