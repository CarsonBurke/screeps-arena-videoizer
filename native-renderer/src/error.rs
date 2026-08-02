use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O failure: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid ReplayIR JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("ReplayIR artifact is not the canonical JSON emitted by the compiler")]
    NonCanonicalJson,

    #[error("{0}")]
    Invalid(String),

    #[error("arithmetic overflow while planning the replay timeline")]
    ArithmeticOverflow,
}

pub type Result<T> = std::result::Result<T, Error>;
