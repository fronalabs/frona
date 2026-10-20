use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("connection closed")]
    Disconnected,

    #[error("no active page")]
    NoActivePage,

    #[error("snapshot index {0} not found; call snapshot() first")]
    UnknownSnapshotIndex(usize),

    #[error("invalid target: must supply selector or snapshot index, not both")]
    InvalidTarget,

    #[error("operation timed out after {0:?}")]
    Timeout(std::time::Duration),

    #[error("tool {tool}: {message}")]
    ToolFailed { tool: &'static str, message: String },

    #[error(transparent)]
    Cdp(#[from] chromiumoxide::error::CdpError),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    /// Initial connection retries are narrower than operation-level recovery:
    /// authentication, TLS, malformed protocol and timeout errors are not disconnects.
    pub fn is_retryable_connect_error(&self) -> bool {
        use chromiumoxide::error::CdpError;
        use tungstenite::{Error as WsError, error::ProtocolError};
        match self {
            Self::Disconnected | Self::Cdp(CdpError::ChannelSendError(_)) => true,
            Self::Cdp(CdpError::Ws(WsError::ConnectionClosed | WsError::AlreadyClosed)) => true,
            Self::Cdp(CdpError::Ws(WsError::Protocol(
                ProtocolError::ResetWithoutClosingHandshake
                | ProtocolError::HandshakeIncomplete
                | ProtocolError::SendAfterClosing,
            ))) => true,
            Self::Cdp(CdpError::Ws(WsError::Io(error))) => matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
            ),
            _ => false,
        }
    }

    pub fn is_disconnect(&self) -> bool {
        if matches!(self, Error::Disconnected | Error::NoActivePage) {
            return true;
        }
        if let Error::Cdp(cdp) = self {
            return is_cdp_disconnect(cdp);
        }
        false
    }
}

/// Shared by the handler loop and the public `is_disconnect` so they
/// classify identically.
pub(crate) fn is_cdp_disconnect(e: &chromiumoxide::error::CdpError) -> bool {
    use chromiumoxide::error::CdpError;
    matches!(
        e,
        CdpError::Ws(_) | CdpError::ChannelSendError(_) | CdpError::Timeout | CdpError::NoResponse
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chromiumoxide::error::CdpError;
    use tungstenite::{Error as WsError, error::ProtocolError};

    #[test]
    fn connect_retry_accepts_only_transport_interruptions() {
        for error in [
            WsError::ConnectionClosed,
            WsError::AlreadyClosed,
            WsError::Protocol(ProtocolError::HandshakeIncomplete),
            WsError::Protocol(ProtocolError::SendAfterClosing),
            WsError::Protocol(ProtocolError::ResetWithoutClosingHandshake),
            WsError::Io(std::io::ErrorKind::ConnectionReset.into()),
            WsError::Io(std::io::ErrorKind::UnexpectedEof.into()),
        ] {
            assert!(Error::Cdp(CdpError::Ws(error)).is_retryable_connect_error());
        }
        for error in [
            CdpError::Timeout,
            CdpError::NoResponse,
            CdpError::Ws(WsError::Io(std::io::ErrorKind::PermissionDenied.into())),
            CdpError::Ws(WsError::Io(std::io::ErrorKind::ConnectionRefused.into())),
            CdpError::Ws(WsError::Protocol(ProtocolError::InvalidOpcode(7))),
        ] {
            assert!(!Error::Cdp(error).is_retryable_connect_error());
        }
        assert!(!Error::NoActivePage.is_retryable_connect_error());
    }
}
