use thiserror::Error;

#[non_exhaustive]
#[derive(Debug, Error)]
pub enum VncError {
    #[error("Auth is required but no password provided")]
    NoPassword,
    #[error("No VNC encoding selected")]
    NoEncoding,
    #[error("Unknown VNC security type: {0}")]
    InvalidSecurityType(u32),
    #[error("Unknown VNC security result: {0}")]
    InvalidSecurityResult(u32),
    #[error("Unknown VNC encoding: {0}")]
    InvalidEncoding(i32),
    #[error("Unsupported VNC security type")]
    UnsupportedSecurityType,
    #[error("Server did not offer the required VNC security type: {0}")]
    RequiredSecurityTypeUnavailable(&'static str),
    #[error("Wrong password")]
    WrongPassword,
    #[error("VNC credential is too long for {field}: {actual} > {limit} bytes")]
    CredentialTooLong {
        field: &'static str,
        actual: usize,
        limit: usize,
    },
    #[error("RA2 server key length is outside the supported range: {actual} bits (expected {min}..={max})")]
    InvalidRa2KeyLength { actual: u32, min: u32, max: u32 },
    #[error("Invalid RA2 RSA public key")]
    InvalidRa2PublicKey,
    #[error("Invalid RA2 encrypted random length: {actual} bytes, expected {expected}")]
    InvalidRa2EncryptedRandomLength { actual: usize, expected: usize },
    #[error("Invalid RA2 decrypted random length: {0} bytes")]
    InvalidRa2RandomLength(usize),
    #[error("RA2 server public-key hash verification failed")]
    Ra2ServerHashMismatch,
    #[error("Unknown RA2 authentication subtype: {0}")]
    InvalidRa2Subtype(u8),
    #[error("RA2 server key verification callback is required")]
    Ra2ServerKeyVerifierRequired,
    #[error("RA2 server key was rejected: {0}")]
    Ra2ServerKeyRejected(String),
    #[error("Invalid RA2 record size limit: {0}")]
    InvalidRa2RecordLimit(usize),
    #[error("RA2 cryptographic operation failed: {0}")]
    Ra2Crypto(&'static str),
    #[error("Server rejected the connection: {0}")]
    SecurityFailure(String),
    #[error("Protocol limit exceeded for {field}: {actual} > {limit}")]
    LimitExceeded {
        field: &'static str,
        actual: u64,
        limit: u64,
    },
    #[error("Invalid framebuffer or rectangle dimensions")]
    InvalidDimensions,
    #[error("Integer overflow while calculating {0}")]
    IntegerOverflow(&'static str),
    #[error("Connect error with unknown reason")]
    ConnectError,
    #[error("Unknown pixel format")]
    WrongPixelFormat,
    #[error("Unkonw server message")]
    WrongServerMessage,
    #[error("Image data cannot be decoded correctly")]
    InvalidImageData,
    #[error("The VNC client isn't started. Or it is already closed")]
    ClientNotRunning,
    #[error(transparent)]
    IoError(#[from] std::io::Error),
    #[error("VNC Error with message: {0}")]
    General(String),
}

impl<T> From<tokio::sync::mpsc::error::SendError<T>> for VncError {
    fn from(_value: tokio::sync::mpsc::error::SendError<T>) -> Self {
        VncError::General("Channel closed".to_string())
    }
}
