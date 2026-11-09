#![cfg(target_os = "android")]
//! Android platform.
//!
//! * File-backed stores use the shared POSIX mmap/`flock` primitives.
//! * Process-mode checking uses `fcntl` OFD record locks at byte range 1/2.
//! * Legacy `<md5>` file names are migrated to the current naming scheme.
//! * Ashmem regions are supported through `ASharedMemory_create` (with a
//!   `/dev/ashmem` fallback).
//!
//! `AndroidPlatform::from_java` resolves `Context.getFilesDir()` and
//! `getCacheDir()` through JNI; apps can also pass explicit paths.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use md5::{Digest, Md5};
use mmkv_platform::{AppState, FileLockKind, LockMode, LockOutcome, MappedRegion, Platform};
use mmkv_platform_posix::PosixPlatform;

const OFD_SETLK: libc::c_int = 37;
const OFD_SETLKW: libc::c_int = 38;

/// Lock byte range used by MMKV's process-mode check.
const MODE_LOCK_OFFSET: libc::off_t = 1;
const MODE_LOCK_LEN: libc::off_t = 2;

fn md5_hex(data: &[u8]) -> String {
    let digest = Md5::digest(data);
    let mut out = String::with_capacity(32);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn encode_file_path(id: &str) -> String {
    const SPECIAL: &[char] = &['\\', '/', ':', '*', '?', '"', '<', '>', '|'];
    if id.chars().any(|ch| SPECIAL.contains(&ch)) {
        format!("specialCharacter/{}", md5_hex(id.as_bytes()))
    } else {
        id.to_owned()
    }
}

fn crc_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".crc");
    PathBuf::from(value)
}

#[derive(Debug, Clone)]
pub struct AndroidPlatform {
    inner: PosixPlatform,
    root: PathBuf,
    cache_dir: PathBuf,
    page_size: usize,
}

impl AndroidPlatform {
    pub fn new(root: impl Into<PathBuf>, cache_dir: impl Into<PathBuf>) -> Self {
        let inner = PosixPlatform::new(None);
        let page_size = inner.page_size();
        Self {
            inner,
            root: root.into(),
            cache_dir: cache_dir.into(),
            page_size,
        }
    }

    /// Resolve `Context.getFilesDir()/mmkv` and `Context.getCacheDir()` through
    /// JNI. The caller owns the `JavaVM`/`Context`; both are only borrowed here.
    #[cfg(target_os = "android")]
    pub fn from_java(vm: &jni::JavaVM, context: &jni::objects::JObject<'_>) -> io::Result<Self> {
        let mut env = vm
            .get_env()
            .map_err(|error| io::Error::other(format!("JavaVM::get_env: {error}")))?;

        let files_dir = java_file_path(&mut env, context, "getFilesDir")?;
        let cache_dir = java_file_path(&mut env, context, "getCacheDir")?;
        Ok(Self::new(files_dir.join("mmkv"), cache_dir))
    }

    /// Fallback constructor for Rust Android apps that do not pass a JNI
    /// `Context`. Reads `MMKV_ROOT`/`MMKV_CACHE`, defaulting to `/data/local/tmp`.
    pub fn from_env() -> Self {
        let root = std::env::var_os("MMKV_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/data/local/tmp/mmkv"));
        let cache = std::env::var_os("MMKV_CACHE")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                root.parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("/data/local/tmp"))
            });
        Self::new(root, cache)
    }

    pub fn root_dir(&self) -> &Path {
        &self.root
    }

    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }
}

#[cfg(target_os = "android")]
fn java_file_path(
    env: &mut jni::JNIEnv<'_>,
    context: &jni::objects::JObject<'_>,
    method: &str,
) -> io::Result<PathBuf> {
    let file = env
        .call_method(context, method, "()Ljava/io/File;", &[])
        .map_err(|error| io::Error::other(format!("{method}: {error}")))?
        .l()
        .map_err(|error| io::Error::other(format!("{method}: {error}")))?;
    let path = env
        .call_method(&file, "getAbsolutePath", "()Ljava/lang/String;", &[])
        .map_err(|error| io::Error::other(format!("{method}: {error}")))?
        .l()
        .map_err(|error| io::Error::other(format!("{method}: {error}")))?;
    let string: jni::objects::JString = path.into();
    let value: String = env
        .get_string(&string)
        .map_err(|error| io::Error::other(format!("{method}: {error}")))?
        .into();
    Ok(PathBuf::from(value))
}

impl Platform for AndroidPlatform {
    fn name(&self) -> &'static str {
        "android"
    }

    fn default_root_dir(&self) -> io::Result<PathBuf> {
        Ok(self.root.clone())
    }

    fn shared_root_dir(&self, group: Option<&str>) -> io::Result<PathBuf> {
        match group {
            Some(group) => Ok(PathBuf::from(group).join("mmkv")),
            None => Ok(self.root.clone()),
        }
    }

    fn temp_dir(&self) -> io::Result<PathBuf> {
        Ok(self.cache_dir.clone())
    }

    fn page_size(&self) -> usize {
        self.page_size
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

    fn lock_file_kind(
        &self,
        file: &File,
        mode: LockMode,
        wait: bool,
        kind: FileLockKind,
    ) -> io::Result<LockOutcome> {
        match kind {
            FileLockKind::Advisory => mmkv_platform_posix::lock_file(file, mode, wait),
            FileLockKind::Record => {
                let lock_type = match mode {
                    LockMode::Shared => libc::F_RDLCK as libc::c_int,
                    LockMode::Exclusive => libc::F_WRLCK as libc::c_int,
                };
                record_lock(file, lock_type as libc::c_int, wait, 0, 0)
            }
        }
    }

    fn unlock_file_kind(&self, file: &File, mode: LockMode, kind: FileLockKind) -> io::Result<()> {
        match kind {
            FileLockKind::Advisory => mmkv_platform_posix::unlock_file(file, mode),
            FileLockKind::Record => {
                record_lock(file, libc::F_UNLCK as libc::c_int, true, 0, 0)?;
                Ok(())
            }
        }
    }

    fn check_process_mode(
        &self,
        crc_path: &Path,
        multi_process: bool,
    ) -> io::Result<(bool, Option<Box<dyn Send + Sync>>)> {
        let file = OpenOptions::new().read(true).write(true).open(crc_path)?;
        if multi_process {
            if record_lock(
                &file,
                libc::F_WRLCK as libc::c_int,
                false,
                MODE_LOCK_OFFSET,
                MODE_LOCK_LEN,
            )? == LockOutcome::Acquired
            {
                return Ok((true, Some(Box::new(file))));
            }
            if record_lock(
                &file,
                libc::F_RDLCK as libc::c_int,
                false,
                MODE_LOCK_OFFSET,
                MODE_LOCK_LEN,
            )? == LockOutcome::Acquired
            {
                return Ok((false, None));
            }
            // Another multi-process instance holds the exclusive marker.
            Ok((true, None))
        } else {
            match record_lock(
                &file,
                libc::F_RDLCK as libc::c_int,
                false,
                MODE_LOCK_OFFSET,
                MODE_LOCK_LEN,
            )? {
                LockOutcome::Acquired => Ok((true, Some(Box::new(file)))),
                LockOutcome::WouldBlock => Ok((false, None)),
            }
        }
    }

    fn migrate_legacy_store(&self, root: &Path, id: &str) -> io::Result<()> {
        let legacy_name = md5_hex(format!("{}/{}", root.display(), id).as_bytes());
        let legacy = root.join(legacy_name);
        let current = root.join(encode_file_path(id));
        if !legacy.exists() || current.exists() {
            return Ok(());
        }
        if let Some(parent) = current.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&legacy, &current)?;
        let legacy_crc = crc_path(&legacy);
        if legacy_crc.exists() {
            std::fs::rename(legacy_crc, crc_path(&current))?;
        }
        Ok(())
    }

    fn open_ashmem(&self, name: &str, size: usize) -> io::Result<Box<dyn MappedRegion>> {
        #[cfg(target_os = "android")]
        {
            ashmem::create(name, size, self.page_size)
        }
        #[cfg(not(target_os = "android"))]
        {
            let _ = (name, size);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "ashmem is only available on Android",
            ))
        }
    }

    fn random_bytes(&self, out: &mut [u8]) -> io::Result<()> {
        mmkv_platform_posix::random_bytes(out)
    }

    fn now_unix_seconds(&self) -> u32 {
        mmkv_platform_posix::now_unix_seconds()
    }

    fn app_state(&self) -> AppState {
        AppState::Foreground
    }
}

fn record_lock(
    file: &File,
    lock_type: libc::c_int,
    wait: bool,
    start: libc::off_t,
    len: libc::off_t,
) -> io::Result<LockOutcome> {
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = lock_type as libc::c_short;
    lock.l_whence = libc::SEEK_SET as libc::c_short;
    lock.l_start = start;
    lock.l_len = len;
    let command = if wait { OFD_SETLKW } else { OFD_SETLK };
    // SAFETY: valid descriptor and flock structure.
    let ret = unsafe { libc::fcntl(file.as_raw_fd(), command, &mut lock) };
    if ret == 0 {
        return Ok(LockOutcome::Acquired);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(code) if code == libc::EAGAIN || code == libc::EACCES => Ok(LockOutcome::WouldBlock),
        _ => Err(error),
    }
}

#[cfg(target_os = "android")]
mod ashmem {
    use std::ffi::{CStr, CString, c_char, c_int};
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd};

    use memmap2::MmapMut;
    use mmkv_platform::{LockMode, LockOutcome, MappedRegion, SyncMode, round_up_to_page};

    const ASHMEM_SET_NAME: libc::c_int = 0x4108;
    const ASHMEM_SET_SIZE: libc::c_int = 0x4008;

    type ASharedMemoryCreate = unsafe extern "C" fn(*const c_char, usize) -> c_int;

    fn create_fd(name: &str, size: usize) -> io::Result<File> {
        let name = CString::new(name)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "ashmem name contains NUL"))?;
        if let Some(fd) = create_via_libandroid(&name, size) {
            if fd >= 0 {
                // SAFETY: fd is newly created and owned here.
                return Ok(unsafe { File::from_raw_fd(fd) });
            }
        }

        let path = c"/dev/ashmem";
        // SAFETY: plain libc open/ioctl with a valid path.
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: valid fd and name pointer.
        if unsafe { libc::ioctl(fd, ASHMEM_SET_NAME, name.as_ptr()) } < 0
            || unsafe { libc::ioctl(fd, ASHMEM_SET_SIZE, size) } < 0
        {
            let error = io::Error::last_os_error();
            // SAFETY: fd is owned here.
            unsafe { libc::close(fd) };
            return Err(error);
        }
        // SAFETY: fd is newly created and owned here.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn create_via_libandroid(name: &CStr, size: usize) -> Option<c_int> {
        // SAFETY: dlopen/dlsym with static NUL-terminated names.
        let handle = unsafe { libc::dlopen(c"libandroid.so".as_ptr(), libc::RTLD_NOW) };
        if handle.is_null() {
            return None;
        }
        // SAFETY: symbol name is NUL-terminated.
        let symbol = unsafe { libc::dlsym(handle, c"ASharedMemory_create".as_ptr()) };
        if symbol.is_null() {
            // SAFETY: handle came from dlopen.
            unsafe { libc::dlclose(handle) };
            return None;
        }
        // SAFETY: signature matches ASharedMemory_create.
        let create: ASharedMemoryCreate = unsafe { std::mem::transmute(symbol) };
        // SAFETY: name is valid for the duration of the call.
        let fd = unsafe { create(name.as_ptr(), size) };
        // SAFETY: handle came from dlopen.
        unsafe { libc::dlclose(handle) };
        Some(fd)
    }

    pub struct AshmemRegion {
        file: File,
        map: MmapMut,
        size: usize,
        page_size: usize,
    }

    pub fn create(name: &str, size: usize, page_size: usize) -> io::Result<Box<dyn MappedRegion>> {
        let size = round_up_to_page(size.max(1), page_size).max(page_size);
        let file = create_fd(name, size)?;
        // SAFETY: ashmem fd is valid and fixed-size.
        let map = unsafe { memmap2::MmapOptions::new().len(size).map_mut(&file)? };
        Ok(Box::new(AshmemRegion {
            file,
            map,
            size,
            page_size,
        }))
    }

    impl MappedRegion for AshmemRegion {
        fn size(&self) -> usize {
            self.size
        }

        fn as_slice(&self) -> &[u8] {
            &self.map
        }

        fn as_mut_slice(&mut self) -> &mut [u8] {
            &mut self.map
        }

        fn resize(&mut self, new_size: usize) -> io::Result<()> {
            if round_up_to_page(new_size.max(1), self.page_size) == self.size {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "ashmem regions have a fixed size",
                ))
            }
        }

        fn sync(&self, mode: SyncMode) -> io::Result<()> {
            match mode {
                SyncMode::Sync => self.map.flush(),
                SyncMode::Async => self.map.flush_async(),
            }
        }

        fn lock(&self, mode: LockMode, wait: bool) -> io::Result<LockOutcome> {
            let lock_type = match mode {
                LockMode::Shared => libc::F_RDLCK as libc::c_int,
                LockMode::Exclusive => libc::F_WRLCK as libc::c_int,
            };
            let mut lock: libc::flock = unsafe { std::mem::zeroed() };
            lock.l_type = lock_type as libc::c_short;
            lock.l_whence = libc::SEEK_SET as libc::c_short;
            let command = if wait { libc::F_SETLKW } else { libc::F_SETLK };
            // SAFETY: valid fd and flock structure.
            let ret = unsafe { libc::fcntl(self.file.as_raw_fd(), command, &mut lock) };
            if ret == 0 {
                Ok(LockOutcome::Acquired)
            } else {
                let error = io::Error::last_os_error();
                match error.raw_os_error() {
                    Some(code) if code == libc::EAGAIN || code == libc::EACCES => {
                        Ok(LockOutcome::WouldBlock)
                    }
                    _ => Err(error),
                }
            }
        }

        fn unlock(&self, _mode: LockMode) -> io::Result<()> {
            let mut lock: libc::flock = unsafe { std::mem::zeroed() };
            lock.l_type = libc::F_UNLCK as libc::c_short;
            lock.l_whence = libc::SEEK_SET as libc::c_short;
            // SAFETY: valid fd and flock structure.
            let ret = unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_SETLK, &mut lock) };
            if ret == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        }

        fn file(&self) -> Option<&File> {
            Some(&self.file)
        }
    }
}
