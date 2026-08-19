pub mod api;
mod client;
pub mod crypto;
mod request;
mod session;

pub use client::NcmClient;
pub use request::{ApiSpec, CryptoMode, PreparedRequest, RawApiResponse, RequestEntropy};
pub use session::SessionConfig;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum NcmError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("NCM API payload must be a JSON object")]
    PayloadNotObject,

    #[error("invalid HTTP header value: {0}")]
    InvalidHeader(#[from] reqwest::header::InvalidHeaderValue),

    #[error("invalid base URL: {0}")]
    InvalidUrl(#[from] url::ParseError),

    #[error("HTTP transport error: {0}")]
    Transport(#[from] reqwest::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("cryptography error: {0}")]
    Crypto(#[from] crypto::CryptoError),

    #[error("NCM returned HTTP {status}: {body}")]
    HttpStatus {
        status: reqwest::StatusCode,
        body: String,
    },

    #[error("download returned HTTP {status} for {url}")]
    DownloadStatus {
        status: reqwest::StatusCode,
        url: String,
    },
}

pub type Result<T> = std::result::Result<T, NcmError>;
