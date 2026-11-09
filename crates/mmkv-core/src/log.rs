//! Logging and the global handler registry.

use std::cell::Cell;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, RwLock};

use crate::handler::{Handler, Level};

static LOG_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);
static HANDLER: RwLock<Option<Arc<dyn Handler>>> = RwLock::new(None);

thread_local! {
    /// Non-zero while a handler callback is running on this thread.
    static IN_HANDLER: Cell<u32> = const { Cell::new(0) };
}

/// True while a handler callback is executing on the current thread.
pub(crate) fn in_handler() -> bool {
    IN_HANDLER.with(|depth| depth.get() > 0)
}

/// Marks a handler callback as running. `enter` returns `None` when a callback
/// is already active on this thread, so nested logging/notifications can be
/// skipped instead of recursing.
pub(crate) struct HandlerGuard;

impl HandlerGuard {
    pub(crate) fn enter() -> Option<Self> {
        IN_HANDLER.with(|depth| {
            if depth.get() > 0 {
                None
            } else {
                depth.set(depth.get() + 1);
                Some(HandlerGuard)
            }
        })
    }
}

impl Drop for HandlerGuard {
    fn drop(&mut self) {
        IN_HANDLER.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

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
        // A handler that logs from inside its own callback would recurse
        // forever; drop the nested message instead.
        if let Some(_guard) = HandlerGuard::enter() {
            handler.log(level, file, line, function, message);
        }
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
