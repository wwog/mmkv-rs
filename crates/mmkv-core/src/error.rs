//! Error types shared by the MMKV engine.

use std::fmt;
use std::io;

pub type Result<T> = std::result::Result<T, Error>;

/// What MMKV calls `MMKVRecoverStrategic`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recover {
    Discard,
    Recover,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CorruptKind {
    Crc,
    Length,
}

#[derive(Debug)]
pub enum Error {
    NotInitialized,
    InvalidId(String),
    Closed,
    ReadOnly,
    FileNotValid { path: String },
    Io(io::Error),
    Corrupted(CorruptKind),
    TooLarge { key: String, size: usize },
    ItemSizeLimit { size: usize, limit: u32 },
    Unsupported(&'static str),
    LockBusy,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotInitialized => write!(f, "MMKV is not initialized"),
            Error::InvalidId(id) => write!(f, "invalid MMKV id: {id:?}"),
            Error::Closed => write!(f, "MMKV instance is closed"),
            Error::ReadOnly => write!(f, "MMKV instance is read-only"),
            Error::FileNotValid { path } => write!(f, "MMKV file is not valid: {path}"),
            Error::Io(err) => write!(f, "I/O error: {err}"),
            Error::Corrupted(kind) => write!(f, "MMKV data is corrupted ({kind:?})"),
            Error::TooLarge { key, size } => {
                write!(f, "value for key {key:?} is too large ({size} bytes)")
            }
            Error::ItemSizeLimit { size, limit } => {
                write!(f, "item of {size} bytes exceeds the limit of {limit} bytes")
            }
            Error::Unsupported(what) => write!(f, "unsupported: {what}"),
            Error::LockBusy => write!(f, "MMKV file lock is busy"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Io(err)
    }
}

impl From<crate::codec::CodecError> for Error {
    fn from(err: crate::codec::CodecError) -> Self {
        match err {
            crate::codec::CodecError::OutOfBounds | crate::codec::CodecError::Malformed => {
                Error::Corrupted(CorruptKind::Length)
            }
            crate::codec::CodecError::LengthOverflow => Error::Corrupted(CorruptKind::Length),
        }
    }
}
