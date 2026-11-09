#![cfg(target_os = "linux")]
//! Linux platform.
//!
//! MMKV's POSIX build has no implicit root directory: callers must provide one
//! (the POSIX demo uses `/tmp/mmkv`). This crate adds the Linux-specific copy
//! path (`sendfile`) on top of the shared POSIX primitives.

use std::fs::File;
#[cfg(target_os = "linux")]
use std::fs::OpenOptions;
use std::io;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use mmkv_platform::{AppState, LockMode, LockOutcome, MappedRegion, Platform};
use mmkv_platform_posix::PosixPlatform;

#[derive(Debug, Clone)]
pub struct LinuxPlatform {
    inner: PosixPlatform,
    root: Option<PathBuf>,
}

impl LinuxPlatform {
    /// `root` is required for the default root; MMKV has no implicit POSIX root.
    pub fn new(root: Option<PathBuf>) -> Self {
        Self {
            inner: PosixPlatform::new(root.clone()),
            root,
        }
    }

    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self::new(Some(root.into()))
    }
}

impl Platform for LinuxPlatform {
    fn name(&self) -> &'static str {
        "linux"
    }

    fn default_root_dir(&self) -> io::Result<PathBuf> {
        self.root.clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "MMKV on Linux requires an explicit root directory",
            )
        })
    }

    fn shared_root_dir(&self, group: Option<&str>) -> io::Result<PathBuf> {
        match group {
            Some(group) => Ok(PathBuf::from(group).join("mmkv")),
            None => self.default_root_dir(),
        }
    }

    fn temp_dir(&self) -> io::Result<PathBuf> {
        Ok(std::env::temp_dir())
    }

    fn page_size(&self) -> usize {
        self.inner.page_size()
    }

    fn open_region(
        &self,
        path: &Path,
        expected_size: usize,
        read_only: bool,
        mayfly_fd: bool,
    ) -> io::Result<Box<dyn MappedRegion>> {
        self.inner
            .open_region(path, expected_size, read_only, mayfly_fd)
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn lock_file(&self, file: &File, mode: LockMode, wait: bool) -> io::Result<LockOutcome> {
        mmkv_platform_posix::lock_file(file, mode, wait)
    }

    fn unlock_file(&self, file: &File, mode: LockMode) -> io::Result<()> {
        mmkv_platform_posix::unlock_file(file, mode)
    }

    fn random_bytes(&self, out: &mut [u8]) -> io::Result<()> {
        mmkv_platform_posix::random_bytes(out)
    }

    fn now_unix_seconds(&self) -> u32 {
        mmkv_platform_posix::now_unix_seconds()
    }

    fn copy_file(&self, src: &Path, dst: &Path, preserve_inode: bool) -> io::Result<()> {
        if !preserve_inode {
            std::fs::copy(src, dst)?;
            return Ok(());
        }
        sendfile_copy(src, dst)
    }

    fn app_state(&self) -> AppState {
        AppState::Foreground
    }
}

#[cfg(target_os = "linux")]
fn sendfile_copy(src: &Path, dst: &Path) -> io::Result<()> {
    let source = File::open(src)?;
    let size = source.metadata()?.len();
    let target = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dst)?;

    let mut offset: libc::off_t = 0;
    let mut remaining = size as usize;
    while remaining > 0 {
        // SAFETY: both descriptors are valid; offset/count are in range.
        let written = unsafe {
            libc::sendfile(
                target.as_raw_fd(),
                source.as_raw_fd(),
                &mut offset,
                remaining,
            )
        };
        if written < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if written == 0 {
            break;
        }
        remaining -= written as usize;
    }
    target.set_len(size)?;
    target.sync_all()?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn sendfile_copy(src: &Path, dst: &Path) -> io::Result<()> {
    std::fs::copy(src, dst)?;
    Ok(())
}
