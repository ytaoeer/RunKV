use tonic::Status;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("storage error: {0}")]
    StorageError(#[from] runkv_storage::Error),
    #[error("invalid watermark: [current: {0}] [new: {1}]")]
    InvalidWatermark(u64, u64),
    #[error("transport error: {0}")]
    TransportError(#[from] tonic::transport::Error),
    #[error("rpc status error: {0}")]
    RpcStatus(#[from] Status),
    #[error("config error: {0}")]
    ConfigError(String),
    #[error("other: {0}")]
    Other(String),
}

impl Error {
    pub fn err(e: impl Into<Box<dyn std::error::Error>>) -> Error {
        Error::Other(e.into().to_string())
    }

    pub fn config_err(e: impl Into<Box<dyn std::error::Error>>) -> Error {
        Error::ConfigError(e.into().to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
