//! Global handler callbacks, mirroring `MMKVHandler.h`.

use crate::error::Recover;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug = 0,
    Info = 1,
    Warning = 2,
    Error = 3,
    None = 4,
}

impl Level {
    pub(crate) fn from_i32(value: i32) -> Self {
        match value {
            0 => Level::Debug,
            1 => Level::Info,
            2 => Level::Warning,
            3 => Level::Error,
            _ => Level::None,
        }
    }
}

/// Unified callback interface. Every method is optional; the defaults match
/// MMKV's behaviour (`Discard` on CRC/length errors, no logging).
pub trait Handler: Send + Sync {
    fn on_crc_check_fail(&self, _mmap_id: &str) -> Recover {
        Recover::Discard
    }

    fn on_file_length_error(&self, _mmap_id: &str) -> Recover {
        Recover::Discard
    }

    fn log(&self, _level: Level, _file: &str, _line: u32, _function: &str, _message: &str) {}

    fn on_content_changed(&self, _mmap_id: &str) {}

    fn on_content_loaded(&self, _mmap_id: &str) {}
}

/// A handler that does nothing.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopHandler;

impl Handler for NoopHandler {}
