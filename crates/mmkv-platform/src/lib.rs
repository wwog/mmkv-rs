//! Platform contract for the MMKV Rust port.
//!
//! This crate contains no OS-specific code. Each supported system provides a
//! [`Platform`] implementation (see `mmkv-platform-macos`,
//! `mmkv-platform-ios`, `mmkv-platform-linux`, ...). The core engine only talks
//! to the traits defined here, mirroring the way GPUI isolates platform work
//! behind a single `Platform` trait.
//!
//! The contract intentionally stays dyn-safe: `mmkv-core` holds an
//! `Arc<dyn Platform>`, so tests and embedders can substitute their own
//! implementation without monomorphising the whole engine.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

/// How a file lock is acquired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockMode {
    Shared,
    Exclusive,
}

/// Result of a non-blocking lock attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockOutcome {
    Acquired,
    WouldBlock,
}

/// `msync(2)` behaviour: `MMKV_SYNC` maps to `Sync`, `MMKV_ASYNC` to `Async`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncMode {
    Sync,
    Async,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppState {
    Foreground,
    Background,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppEvent {
    EnterBackground,
    BecameActive,
    MemoryWarning,
}

/// A byte-addressable, resizable, shared-memory region backed by a file (or,
/// on Android, by ashmem).
///
/// Implementations must guarantee that:
/// * `size()` is always a multiple of the platform page size (for writable
///   regions), matching MMKV's `MemoryFile::truncate`.
/// * growing zero-fills the new bytes explicitly; a failed grow rolls the file
///   back to its previous size.
/// * the address of the mapping may change after `resize`; callers must not
///   hold borrows across it.
pub trait MappedRegion: Send + Sync {
    fn size(&self) -> usize;
    fn as_slice(&self) -> &[u8];
    /// Only valid for writable regions.
    fn as_mut_slice(&mut self) -> &mut [u8];
    /// Resize the region. `new_size` is rounded up to the page size by the
    /// implementation. Growth is zero-filled.
    fn resize(&mut self, new_size: usize) -> io::Result<()>;
    fn sync(&self, mode: SyncMode) -> io::Result<()>;
    fn lock(&self, mode: LockMode, wait: bool) -> io::Result<LockOutcome>;
    fn unlock(&self, mode: LockMode) -> io::Result<()>;
    /// The backing file descriptor, if one is kept open. Single-process MMKV
    /// instances may close it after mapping ("mayfly fd").
    fn file(&self) -> Option<&File>;
}

pub trait Platform: Send + Sync {
    /// Stable platform name, e.g. `"macos"`.
    fn name(&self) -> &'static str;

    /// Default root directory for MMKV stores, including the trailing `mmkv`
    /// component (MMKV's `+[MMKV mmkvBasePath]`).
    fn default_root_dir(&self) -> io::Result<PathBuf>;

    /// App Group / shared container root, including the trailing `mmkv`
    /// component. `group` is platform-specific (an App Group identifier on
    /// Apple platforms).
    fn shared_root_dir(&self, group: Option<&str>) -> io::Result<PathBuf>;

    /// Temporary directory used for atomic copies.
    fn temp_dir(&self) -> io::Result<PathBuf>;

    /// Runtime page size. MMKV rounds every writable file to this size.
    fn page_size(&self) -> usize;

    /// Open (creating if needed) and map a file. `expected_size` is a hint and
    /// is rounded up to a page multiple; `mayfly_fd` requests that the file
    /// descriptor be closed after mapping.
    fn open_region(
        &self,
        path: &Path,
        expected_size: usize,
        read_only: bool,
        mayfly_fd: bool,
    ) -> io::Result<Box<dyn MappedRegion>>;

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    /// iOS: apply `NSFileProtectionCompleteUntilFirstUserAuthentication`.
    /// Other platforms: no-op.
    fn protect_file(&self, _path: &Path) -> io::Result<()> {
        Ok(())
    }

    /// Optional: mark a file as excluded from cloud backups. MMKV itself does
    /// not do this, so the default is a no-op.
    fn exclude_from_backup(&self, _path: &Path) -> io::Result<()> {
        Ok(())
    }

    /// Cryptographically secure random bytes (MMKV uses them for the AES IV).
    fn random_bytes(&self, out: &mut [u8]) -> io::Result<()>;

    /// Current Unix time in seconds, truncated to `u32` like MMKV.
    fn now_unix_seconds(&self) -> u32;

    fn app_state(&self) -> AppState {
        AppState::Foreground
    }

    /// Subscribe to application lifecycle events. Returns a cancellation
    /// callback. Non-Apple platforms use the default no-op.
    fn subscribe_app_events(&self, _cb: Box<dyn Fn(AppEvent) + Send + Sync>) -> Box<dyn FnOnce()> {
        Box::new(|| {})
    }
}

/// Round `size` up to a page multiple, exactly like
/// `MemoryFile::truncate` (`size < page || size % page != 0`).
pub fn round_up_to_page(size: usize, page: usize) -> usize {
    if size < page || !size.is_multiple_of(page) {
        ((size / page) + 1) * page
    } else {
        size
    }
}

/// Compute MMKV's effective initial capacity.
pub fn initial_capacity(expected: usize, page: usize) -> usize {
    round_up_to_page(expected, page).max(page)
}
