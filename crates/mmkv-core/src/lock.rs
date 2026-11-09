//! Recursive shared/exclusive inter-process lock, a port of
//! `InterProcessLock`/`FileLock`.
//!
//! MMKV locks the `.crc` file descriptor. We `dup` that descriptor, so the
//! lock is attached to the same open file description and interoperates with
//! the C++ implementation. The actual primitive is supplied by the platform
//! (`flock` on POSIX, `LockFileEx` on Windows).

use std::fs::File;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use mmkv_platform::{FileLockKind, LockMode, LockOutcome, Platform};
use parking_lot::Mutex;

#[derive(Default)]
struct Counts {
    shared: u32,
    exclusive: u32,
}

pub struct FileLock {
    platform: Option<Arc<dyn Platform>>,
    file: Option<File>,
    kind: FileLockKind,
    enabled: AtomicBool,
    counts: Mutex<Counts>,
}

impl FileLock {
    pub fn disabled() -> Self {
        Self {
            platform: None,
            file: None,
            kind: FileLockKind::Advisory,
            enabled: AtomicBool::new(false),
            counts: Mutex::new(Counts::default()),
        }
    }

    pub fn new(platform: Arc<dyn Platform>, file: File) -> Self {
        Self::new_with_kind(platform, file, FileLockKind::Advisory)
    }

    pub fn new_with_kind(platform: Arc<dyn Platform>, file: File, kind: FileLockKind) -> Self {
        Self {
            platform: Some(platform),
            file: Some(file),
            kind,
            enabled: AtomicBool::new(true),
            counts: Mutex::new(Counts::default()),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// Release every lock count and disable this lock. Used by `Store::close`
    /// so closing an instance frees other processes even while Rust handles
    /// still exist.
    pub fn release_all(&self) {
        if !self.enabled.swap(false, Ordering::AcqRel) {
            return;
        }
        let mut counts = self.counts.lock();
        if counts.shared > 0 || counts.exclusive > 0 {
            let _ = self.funlock(LockMode::Exclusive);
        }
        counts.shared = 0;
        counts.exclusive = 0;
    }

    fn flock(&self, mode: LockMode, wait: bool) -> io::Result<LockOutcome> {
        let platform = self
            .platform
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "file lock is disabled"))?;
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "file lock is disabled"))?;
        platform.lock_file_kind(file, mode, wait, self.kind)
    }

    fn funlock(&self, mode: LockMode) -> io::Result<()> {
        let platform = self
            .platform
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "file lock is disabled"))?;
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "file lock is disabled"))?;
        platform.unlock_file_kind(file, mode, self.kind)
    }

    pub fn lock(&self, mode: LockMode) -> io::Result<()> {
        if !self.is_enabled() {
            return Ok(());
        }
        let mut counts = self.counts.lock();
        if !self.is_enabled() {
            return Ok(());
        }
        match mode {
            LockMode::Shared => {
                if counts.shared == 0 && counts.exclusive == 0 {
                    self.flock(LockMode::Shared, true)?;
                }
                counts.shared += 1;
            }
            LockMode::Exclusive => {
                if counts.exclusive > 0 {
                    counts.exclusive += 1;
                    return Ok(());
                }
                if counts.shared > 0 {
                    // Upgrade: try without blocking first, otherwise drop the
                    // shared lock and block; restore it if the upgrade fails.
                    match self.flock(LockMode::Exclusive, false)? {
                        LockOutcome::Acquired => {}
                        LockOutcome::WouldBlock => {
                            self.funlock(LockMode::Exclusive)?;
                            if let Err(err) = self.flock(LockMode::Exclusive, true) {
                                let _ = self.flock(LockMode::Shared, true);
                                return Err(err);
                            }
                        }
                    }
                } else {
                    self.flock(LockMode::Exclusive, true)?;
                }
                counts.exclusive = 1;
            }
        }
        Ok(())
    }

    pub fn try_lock(&self, mode: LockMode) -> io::Result<bool> {
        if !self.is_enabled() {
            return Ok(true);
        }
        let mut counts = self.counts.lock();
        if !self.is_enabled() {
            return Ok(true);
        }
        match mode {
            LockMode::Shared => {
                if counts.shared > 0 || counts.exclusive > 0 {
                    counts.shared += 1;
                    return Ok(true);
                }
                if self.flock(LockMode::Shared, false)? == LockOutcome::Acquired {
                    counts.shared = 1;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            LockMode::Exclusive => {
                if counts.exclusive > 0 {
                    counts.exclusive += 1;
                    return Ok(true);
                }
                if counts.shared > 0 {
                    if self.flock(LockMode::Exclusive, false)? == LockOutcome::Acquired {
                        counts.exclusive = 1;
                        return Ok(true);
                    }
                    return Ok(false);
                }
                if self.flock(LockMode::Exclusive, false)? == LockOutcome::Acquired {
                    counts.exclusive = 1;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
        }
    }

    pub fn unlock(&self, mode: LockMode) {
        if !self.is_enabled() {
            return;
        }
        let mut counts = self.counts.lock();
        match mode {
            LockMode::Shared => {
                if counts.shared == 0 {
                    return;
                }
                counts.shared -= 1;
                if counts.shared == 0 && counts.exclusive == 0 {
                    let _ = self.funlock(LockMode::Exclusive);
                }
            }
            LockMode::Exclusive => {
                if counts.exclusive == 0 {
                    return;
                }
                counts.exclusive -= 1;
                if counts.exclusive == 0 {
                    if counts.shared > 0 {
                        // Downgrade: flock(LOCK_SH) converts the lock.
                        let _ = self.flock(LockMode::Shared, true);
                    } else {
                        let _ = self.funlock(LockMode::Exclusive);
                    }
                }
            }
        }
    }

    pub fn shared_guard(&self) -> io::Result<ProcessGuard<'_>> {
        self.lock(LockMode::Shared)?;
        Ok(ProcessGuard {
            lock: self,
            mode: LockMode::Shared,
        })
    }

    pub fn exclusive_guard(&self) -> io::Result<ProcessGuard<'_>> {
        self.lock(LockMode::Exclusive)?;
        Ok(ProcessGuard {
            lock: self,
            mode: LockMode::Exclusive,
        })
    }

    pub fn try_exclusive_guard(&self) -> io::Result<Option<ProcessGuard<'_>>> {
        if self.try_lock(LockMode::Exclusive)? {
            Ok(Some(ProcessGuard {
                lock: self,
                mode: LockMode::Exclusive,
            }))
        } else {
            Ok(None)
        }
    }
}

/// Releases the corresponding lock count on drop.
pub struct ProcessGuard<'a> {
    lock: &'a FileLock,
    mode: LockMode,
}

impl Drop for ProcessGuard<'_> {
    fn drop(&mut self) {
        self.lock.unlock(self.mode);
    }
}
