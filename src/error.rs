use std::fmt;

#[derive(Debug)]
pub enum Error {
    Parameter(String),
    Target(String),
    Other(String),
}

impl Error {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Parameter(_) => 252,
            Self::Target(_) => 254,
            Self::Other(_) => 255,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::Parameter(message) | Self::Target(message) | Self::Other(message) => message,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn parameter(message: impl Into<String>) -> Error {
    Error::Parameter(message.into())
}

pub fn target(message: impl Into<String>) -> Error {
    Error::Target(message.into())
}

pub fn other(message: impl Into<String>) -> Error {
    Error::Other(message.into())
}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Error::Other(message)
    }
}
