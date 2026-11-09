//! Logging and the global handler registry.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, RwLock};

use crate::handler::{Handler, Level};

static LOG_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);
static HANDLER: RwLock<Option<Arc<dyn Handler>>> = RwLock::new(None);

pub fn set_level(level: Level) {
    LOG_LEVEL.store(level as u8, Ordering::Release);
}

pub fn level() -> Level {
    Level::from_i32(LOG_LEVEL.load(Ordering::Acquire) as i32)
}

pub fn set_handler(handler: Option<Arc<dyn Handler>>) {
    *HANDLER.write().expect("handler lock poisoned") = handler;
}

pub fn handler() -> Option<Arc<dyn Handler>> {
    HANDLER.read().expect("handler lock poisoned").clone()
}

pub fn log_at(level: Level, file: &str, line: u32, function: &str, message: &str) {
    if level < self::level() || level == Level::None {
        return;
    }
    if let Some(handler) = handler() {
        handler.log(level, file, line, function, message);
        return;
    }
    if level >= Level::Warning {
        eprintln!("[MMKV {level:?}] <{file}:{line}::{function}> {message}");
    }
}

macro_rules! mmkv_log {
    ($level:expr, $($arg:tt)*) => {
        $crate::log::log_at($level, file!(), line!(), module_path!(), &format!($($arg)*))
    };
}

pub(crate) use mmkv_log;
