//! Store configuration, mirroring `MMKVConfig` from `MMKV.h`.

use std::fmt;
use std::path::Path;

use crate::error::Recover;

/// MMKV mode bitmask (`MMKVMode` in `MMKV.h`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Mode(u32);

impl Mode {
    pub const SINGLE_PROCESS: Mode = Mode(1 << 0);
    pub const MULTI_PROCESS: Mode = Mode(1 << 1);
    /// Android `Context.MODE_MULTI_PROCESS` compatibility bit.
    pub const CONTEXT_MODE_MULTI_PROCESS: Mode = Mode(1 << 2);
    pub const ASHMEM: Mode = Mode(1 << 3);
    /// Android `MMKV_BACKUP` (accepted for parity; currently a no-op).
    pub const BACKUP: Mode = Mode(1 << 4);
    pub const READ_ONLY: Mode = Mode(1 << 5);

    pub const fn empty() -> Self {
        Mode(0)
    }

    pub const fn contains(self, other: Mode) -> bool {
        (self.0 & other.0) != 0
    }

    pub const fn is_multi_process(self) -> bool {
        self.contains(Mode::MULTI_PROCESS)
            || self.contains(Mode::CONTEXT_MODE_MULTI_PROCESS)
            || self.contains(Mode::ASHMEM)
    }

    pub const fn is_ashmem(self) -> bool {
        self.contains(Mode::ASHMEM)
    }

    pub const fn is_read_only(self) -> bool {
        self.contains(Mode::READ_ONLY)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }
}

impl Default for Mode {
    fn default() -> Self {
        Mode::SINGLE_PROCESS
    }
}

impl fmt::Debug for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut flags = Vec::new();
        if self.contains(Mode::MULTI_PROCESS) {
            flags.push("MULTI_PROCESS");
        } else {
            flags.push("SINGLE_PROCESS");
        }
        if self.contains(Mode::READ_ONLY) {
            flags.push("READ_ONLY");
        }
        f.debug_tuple("Mode").field(&flags.join("|")).finish()
    }
}

impl std::ops::BitOr for Mode {
    type Output = Mode;

    fn bitor(self, rhs: Mode) -> Mode {
        Mode(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for Mode {
    fn bitor_assign(&mut self, rhs: Mode) {
        self.0 |= rhs.0;
    }
}

/// Automatic key-expiration configuration (`enableKeyExpire` +
/// `expiredInSeconds`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expiration {
    /// Enable expiration, each key carries its own deadline (MMKV `0`).
    PerKey,
    /// Enable expiration and use this duration by default, in seconds.
    After(u32),
}

impl Expiration {
    pub const fn seconds(self) -> u32 {
        match self {
            Expiration::PerKey => 0,
            Expiration::After(seconds) => seconds,
        }
    }
}

/// A raw MMKV AES key.
///
/// MMKV uses the bytes directly (truncated or zero-padded to 16/32 bytes); it
/// does not run a KDF. The wrapper is redacted in `Debug` output.
#[derive(Clone, PartialEq, Eq)]
pub struct CryptKey(Vec<u8>);

impl CryptKey {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        CryptKey(bytes.into())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for CryptKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CryptKey")
            .field("len", &self.0.len())
            .finish_non_exhaustive()
    }
}

impl Drop for CryptKey {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// All-in-one configuration for opening a store (`MMKVConfig`).
#[derive(Clone, Debug, Default)]
pub struct StoreConfig<'a> {
    pub mode: Mode,
    pub crypt_key: Option<&'a CryptKey>,
    pub aes256: bool,
    pub root: Option<&'a Path>,
    pub expected_capacity: usize,
    pub expiration: Option<Expiration>,
    pub compare_before_set: bool,
    pub recover: Option<Recover>,
    pub item_size_limit: u32,
}

impl<'a> StoreConfig<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mode(mut self, mode: Mode) -> Self {
        self.mode = mode;
        self
    }

    pub fn crypt_key(mut self, key: &'a CryptKey, aes256: bool) -> Self {
        self.crypt_key = Some(key);
        self.aes256 = aes256;
        self
    }

    pub fn root(mut self, root: &'a Path) -> Self {
        self.root = Some(root);
        self
    }

    pub fn expected_capacity(mut self, capacity: usize) -> Self {
        self.expected_capacity = capacity;
        self
    }

    pub fn expiration(mut self, expiration: Expiration) -> Self {
        self.expiration = Some(expiration);
        self
    }

    pub fn compare_before_set(mut self, enabled: bool) -> Self {
        self.compare_before_set = enabled;
        self
    }

    pub fn recover(mut self, recover: Recover) -> Self {
        self.recover = Some(recover);
        self
    }

    pub fn item_size_limit(mut self, limit: u32) -> Self {
        self.item_size_limit = limit;
        self
    }
}
