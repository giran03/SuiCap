//! Crate-level error. Each module owns its own `thiserror` enum; this wraps them
//! and answers the one question the pipeline needs: recover or stop?

use crate::{capture::CaptureError, encode::EncodeError};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("capture: {0}")]
    Capture(#[from] CaptureError),
    #[error("encode: {0}")]
    Encode(#[from] EncodeError),
}

impl Error {
    /// Recoverable errors move the pipeline to `Recovering`; fatal ones to `Idle`.
    pub fn is_recoverable(&self) -> bool {
        match self {
            Error::Capture(e) => e.is_recoverable(),
            Error::Encode(e) => e.is_recoverable(),
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
