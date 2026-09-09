//! SIP signaling and fax media with thread-confined native ownership.

#![deny(unsafe_op_in_unsafe_fn)]

mod fax;
mod g711;
mod logging;
mod nat;
mod network;
mod recovery;
pub use recovery::*;
mod sdp;
mod sip;
mod tls;
pub use fax::{
    AudioFax, FRAME_SAMPLES, FaxEvent, FaxFailure, FaxStage, IfpPacket, PacketFax, PageProgress, TransferStats,
};
pub use g711::G711;
pub use sip::{
    Account, AdmissionRequest, Mode, ReceiveConfig, ReceiveEvent, ReceiveState, SendRequest,
    SendUpdate, ServiceAccount, ServiceSend, SignalingTransport, SipService, send,
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("SIP: {0}")]
    Sip(String),
    #[error("Fax cancelled")]
    Cancelled,
    #[error(transparent)]
    Fax(#[from] FaxFailure),
    #[error("SpanDSP: {0}")]
    Dsp(#[from] spandsp::error::SpanDspError),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, Error>;
