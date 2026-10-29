use std::error::Error;
use std::fmt;

/// Errors that can occur within the controller.
#[derive(Debug)]
pub enum ControllerError {
    /// A generic internal error with a descriptive message.
    Internal(String),
    /// An error originating from configuration handling.
    Config(String),
    /// An error originating from I/O operations.
    Io(String),
    /// An error indicating an invalid or unsupported operation.
    InvalidOperation(String),
}

impl fmt::Display for ControllerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ControllerError::Internal(msg) => write!(f, "internal error: {}", msg),
            ControllerError::Config(msg) => write!(f, "config error: {}", msg),
            ControllerError::Io(msg) => write!(f, "io error: {}", msg),
            ControllerError::InvalidOperation(msg) => {
                write!(f, "invalid operation: {}", msg)
            }
        }
    }
}

impl Error for ControllerError {}

impl From<std::io::Error> for ControllerError {
    fn from(err: std::io::Error) -> Self {
        ControllerError::Io(err.to_string())
    }
}
