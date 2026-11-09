//! `mmkv-core`: a byte-compatible Rust port of Tencent MMKV v2.4.3.
//!
//! The crate is platform-agnostic. All OS interaction goes through
//! [`mmkv_platform::Platform`], which platform crates implement.
//!
//! Current scope: the complete plaintext (unencrypted) format, including the
//! append-only log, full write-back, metadata/CRC handling, multi-process
//! refresh, expiration-aware reads and the public get/set API. The AES-CFB
//! layer is the next phase and is intentionally absent from this crate version.

pub mod codec;
pub mod config;
pub mod crc32;
pub mod crypt;
pub mod error;
pub mod handler;
pub mod kv;
pub mod lock;
pub mod log;
pub mod memory;
pub mod meta;
pub mod namespace;
pub mod path;
pub mod registry;
pub mod store;

pub use config::{CryptKey, Expiration, Mode, StoreConfig};
pub use error::{CorruptKind, Error, Recover, Result};
pub use handler::{Handler, Level, NoopHandler};
pub use namespace::Namespace;
pub use store::{Mmkv, Store, StoreLockGuard};

/// MMKV version this port is compatible with.
pub const VERSION: &str = "v2.4.3";

/// Runtime page size as reported by the platform.
pub fn page_size(platform: &dyn mmkv_platform::Platform) -> usize {
    platform.page_size()
}
