//! The typed error surface. Part of the semver contract — variants are `#[non_exhaustive]`.

use crate::{Capability, Protocol, Source};

/// Everything a fetch can fail with. The three financially-meaningful kinds are
/// [`Transient`](Error::Transient) (retryable), [`Integrity`](Error::Integrity) (a required source
/// failed → fail closed), and [`AbsentPrice`](Error::AbsentPrice) (never coerce to `0`/`1`).
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A transient provider failure (HTTP 429, timeout). Safe to retry.
    #[error("transient: {message}")]
    Transient {
        /// Human-readable cause.
        message: String,
        /// Server-advised backoff, if any.
        retry_after: Option<std::time::Duration>,
    },

    /// A permanent failure — a bad address, an unsupported chain, malformed config. Not retryable.
    #[error("permanent: {message}")]
    Permanent {
        /// Human-readable cause.
        message: String,
    },

    /// A required source failed or returned incomplete data. The engine fails closed rather than
    /// return partial financial data.
    #[error("integrity: {message}")]
    Integrity {
        /// Human-readable cause.
        message: String,
    },

    /// No price is available for an asset. Never substituted with `0` or `1`.
    #[error("absent price for asset: {asset}")]
    AbsentPrice {
        /// The asset that could not be priced (symbol or address).
        asset: String,
    },

    /// The requested capability is not offered by any registered backend of this protocol.
    #[error("{protocol:?} does not support {capability:?} via {backend:?}")]
    Unsupported {
        /// The protocol asked for.
        protocol: Protocol,
        /// The capability asked for.
        capability: Capability,
        /// The backend that was routed.
        backend: Source,
    },

    /// An error from an injected provider (HTTP client, RPC transport, custom adapter). Opaque on
    /// purpose so extension points aren't forced into this enum.
    #[error("provider: {0}")]
    Provider(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl Error {
    /// Whether retrying the operation could succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Error::Transient { .. })
    }
}
