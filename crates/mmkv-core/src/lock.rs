//! Recursive shared/exclusive inter-process lock, a port of
//! `InterProcessLock`/`FileLock` on top of `flock(2)`.
//!
//! MMKV locks the `.crc` file descriptor. We `dup` that descriptor, so the
//! `flock` lock is attached to the same open file description and interoperates
//! with the C++ implementation.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

use mmkv_platform::{LockMode, LockOutcome};
use parking_lot::Mutex;

#[derive(Default)]
struct Counts {
    shared: u32,
    exclusive: u32,
}

pub struct FileLock {
    file: Option<File>,
    enabled: bool,
    counts: Mutex<Counts>,
}

impl FileLock {
    pub fn disabled() -> Self {
        Self {
            file: None,
            enabled: false,
            counts: Mutex::new(Counts::default()),
        }
    }

    pub fn new(file: File) -> Self {
        Self {
            file: Some(file),
            enabled: true,
            counts: Mutex::new(Counts::default()),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn flock(&self, mode: LockMode, wait: bool) -> io::Result<LockOutcome> {
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "file lock is disabled"))?;
        let base = match mode {
            LockMode::Shared => libc::LOCK_SH,
            LockMode::Exclusive => libc::LOCK_EX,
        };
        let operation = if wait { base } else { base | libc::LOCK_NB };
        // SAFETY: plain libc call with a valid descriptor.
        let ret = unsafe { libc::flock(file.as_raw_fd(), operation) };
        if ret == 0 {
            return Ok(LockOutcome::Acquired);
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN => {
                Ok(LockOutcome::WouldBlock)
            }
            _ => Err(err),
        }
    }

    fn funlock(&self) -> io::Result<()> {
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "file lock is disabled"))?;
        // SAFETY: plain libc call with a valid descriptor.
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        if ret == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    pub fn lock(&self, mode: LockMode) -> io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let mut counts = self.counts.lock();
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
                            self.funlock()?;
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
        if !self.enabled {
            return Ok(true);
        }
        let mut counts = self.counts.lock();
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
        if !self.enabled {
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
                    let _ = self.funlock();
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
                        let _ = self.funlock();
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
