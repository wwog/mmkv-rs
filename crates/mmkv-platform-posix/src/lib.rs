//! Shared POSIX implementation of the MMKV platform contract.
//!
//! Contains the portable file mapping and advisory locking primitives used by
//! `mmkv-platform-macos`, `mmkv-platform-ios`, `mmkv-platform-linux` and
//! `mmkv-platform-android`. It deliberately mirrors MMKV's `MemoryFile`:
//! `MAP_SHARED`, page-rounded sizes, explicit zero-fill on growth, `msync`, and
//! whole-file `flock`.

use std::fs::{File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use mmkv_platform::{
    AppState, LockMode, LockOutcome, MappedRegion, Platform, SyncMode, initial_capacity,
    round_up_to_page,
};

enum Map {
    ReadOnly(memmap2::Mmap),
    ReadWrite(memmap2::MmapMut),
    Empty,
}

impl Map {
    fn len(&self) -> usize {
        match self {
            Map::ReadOnly(map) => map.len(),
            Map::ReadWrite(map) => map.len(),
            Map::Empty => 0,
        }
    }

    fn as_slice(&self) -> &[u8] {
        match self {
            Map::ReadOnly(map) => map,
            Map::ReadWrite(map) => map,
            Map::Empty => &[],
        }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        match self {
            Map::ReadWrite(map) => map,
            Map::ReadOnly(_) => panic!("attempted to write a read-only MMKV mapping"),
            Map::Empty => &mut [],
        }
    }
}

/// A POSIX `mmap(2)`-backed region.
pub struct PosixMappedRegion {
    file: Option<File>,
    map: Map,
    path: PathBuf,
    read_only: bool,
    page_size: usize,
}

impl std::fmt::Debug for PosixMappedRegion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PosixMappedRegion")
            .field("path", &self.path)
            .field("len", &self.map.len())
            .field("read_only", &self.read_only)
            .finish()
    }
}

/// Open (and create when writable) a file and map it with `MAP_SHARED`.
///
/// For writable files the size is rounded up to a page multiple and the newly
/// extended region is explicitly zeroed, matching MMKV's `MemoryFile`.
pub fn open_region(
    path: &Path,
    expected_size: usize,
    read_only: bool,
    mayfly_fd: bool,
    page_size: usize,
) -> io::Result<Box<dyn MappedRegion>> {
    if read_only {
        let file = File::open(path)?;
        let actual = file.metadata()?.len() as usize;
        if actual == 0 {
            return Ok(Box::new(PosixMappedRegion {
                file: Some(file),
                map: Map::Empty,
                path: path.to_path_buf(),
                read_only: true,
                page_size,
            }));
        }
        // SAFETY: the mapping is never exposed beyond the lifetime of the
        // region and MMKV's contract requires callers not to resize a
        // read-only file.
        let map = unsafe { memmap2::MmapOptions::new().len(actual).map(&file)? };
        return Ok(Box::new(PosixMappedRegion {
            file: Some(file),
            map: Map::ReadOnly(map),
            path: path.to_path_buf(),
            read_only: true,
            page_size,
        }));
    }

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o700)
        .open(path)?;

    let actual = file.metadata()?.len() as usize;
    // Like MMKV's `MemoryFile::reloadFromFile`: keep an existing file that is
    // already large enough and page aligned; only grow (never truncate) on
    // open.
    let mut target = initial_capacity(expected_size, page_size);
    if actual > 0 {
        if !actual.is_multiple_of(page_size) {
            target = round_up_to_page(actual, page_size);
        } else if actual >= page_size {
            target = actual;
        }
    }
    if actual != target {
        if actual > target {
            // Cannot happen with the logic above, but be defensive: never
            // shrink an existing store on open.
            target = actual;
        }
        file.set_len(target as u64)?;
        if target > actual {
            zero_fill(&mut file, actual as u64, target - actual)?;
        }
    }

    // SAFETY: as above; we own the File and keep it open (unless mayfly).
    let map = unsafe { memmap2::MmapOptions::new().len(target).map_mut(&file)? };
    let file = if mayfly_fd { None } else { Some(file) };
    Ok(Box::new(PosixMappedRegion {
        file,
        map: Map::ReadWrite(map),
        path: path.to_path_buf(),
        read_only: false,
        page_size,
    }))
}

fn zero_fill(file: &mut File, start: u64, len: usize) -> io::Result<()> {
    const ZEROS: [u8; 4096] = [0; 4096];
    file.seek(SeekFrom::Start(start))?;
    let mut written = 0usize;
    while written < len {
        let chunk = (len - written).min(ZEROS.len());
        file.write_all(&ZEROS[..chunk])?;
        written += chunk;
    }
    Ok(())
}

impl PosixMappedRegion {
    fn file_for_io(&mut self) -> io::Result<&mut File> {
        if self.file.is_none() {
            let file = OpenOptions::new().read(true).write(true).open(&self.path)?;
            self.file = Some(file);
        }
        Ok(self.file.as_mut().expect("file was just opened"))
    }
}

impl MappedRegion for PosixMappedRegion {
    fn size(&self) -> usize {
        self.map.len()
    }

    fn as_slice(&self) -> &[u8] {
        self.map.as_slice()
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        self.map.as_mut_slice()
    }

    fn resize(&mut self, new_size: usize) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cannot resize a read-only MMKV region",
            ));
        }
        let target = round_up_to_page(new_size, self.page_size);
        let old = self.map.len();
        if target == old {
            return Ok(());
        }

        let file = self.file_for_io()?;
        file.set_len(target as u64)?;
        if target > old {
            // If zero filling fails, try to roll the file back like MMKV does.
            if let Err(err) = zero_fill(file, old as u64, target - old) {
                let _ = file.set_len(old as u64);
                return Err(err);
            }
        }

        // Drop the old mapping before remapping. `resize` takes &mut self, so
        // no borrows into the old mapping can be alive here.
        self.map = Map::Empty;
        // SAFETY: same contract as `open_region`.
        let map = unsafe {
            memmap2::MmapOptions::new()
                .len(target)
                .map_mut(self.file.as_ref().expect("file must be open"))?
        };
        self.map = Map::ReadWrite(map);
        Ok(())
    }

    fn sync(&self, mode: SyncMode) -> io::Result<()> {
        match &self.map {
            Map::ReadOnly(_) | Map::Empty => Ok(()),
            Map::ReadWrite(map) => match mode {
                SyncMode::Sync => map.flush(),
                SyncMode::Async => map.flush_async(),
            },
        }
    }

    fn lock(&self, mode: LockMode, wait: bool) -> io::Result<LockOutcome> {
        let fd = self
            .file
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "mayfly fd closed"))?
            .as_raw_fd();
        let base = match mode {
            LockMode::Shared => libc::LOCK_SH,
            LockMode::Exclusive => libc::LOCK_EX,
        };
        let op = if wait { base } else { base | libc::LOCK_NB };
        // SAFETY: plain libc call with a valid fd.
        let ret = unsafe { libc::flock(fd, op) };
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

    fn unlock(&self, _mode: LockMode) -> io::Result<()> {
        let fd = self
            .file
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "mayfly fd closed"))?
            .as_raw_fd();
        // SAFETY: plain libc call with a valid fd.
        let ret = unsafe { libc::flock(fd, libc::LOCK_UN) };
        if ret == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn file(&self) -> Option<&File> {
        self.file.as_ref()
    }
}

/// A minimal POSIX platform. Platform crates embed this for the generic pieces
/// and override the OS policy (directories, file protection, lifecycle).
#[derive(Debug, Clone)]
pub struct PosixPlatform {
    root: Option<PathBuf>,
    page_size: usize,
}

impl PosixPlatform {
    pub fn new(root: Option<PathBuf>) -> Self {
        Self {
            root,
            page_size: page_size(),
        }
    }

    pub fn with_page_size(root: Option<PathBuf>, page_size: usize) -> Self {
        Self { root, page_size }
    }
}

impl Platform for PosixPlatform {
    fn name(&self) -> &'static str {
        "posix"
    }

    fn default_root_dir(&self) -> io::Result<PathBuf> {
        self.root.clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "no default MMKV root on this platform; pass one explicitly",
            )
        })
    }

    fn shared_root_dir(&self, group: Option<&str>) -> io::Result<PathBuf> {
        match group {
            Some(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "shared containers are not supported on this platform",
            )),
            None => self.default_root_dir(),
        }
    }

    fn temp_dir(&self) -> io::Result<PathBuf> {
        Ok(std::env::temp_dir())
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
        open_region(path, expected_size, read_only, mayfly_fd, self.page_size)
    }

    fn random_bytes(&self, out: &mut [u8]) -> io::Result<()> {
        random_bytes(out)
    }

    fn now_unix_seconds(&self) -> u32 {
        now_unix_seconds()
    }

    fn app_state(&self) -> AppState {
        AppState::Foreground
    }
}

pub fn page_size() -> usize {
    // SAFETY: sysconf is always safe to call.
    let value = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if value <= 0 { 4096 } else { value as usize }
}

pub fn random_bytes(out: &mut [u8]) -> io::Result<()> {
    getrandom::fill(out).map_err(|err| io::Error::other(err.to_string()))
}

pub fn now_unix_seconds() -> u32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().min(u32::MAX as u64) as u32)
        .unwrap_or(0)
}
