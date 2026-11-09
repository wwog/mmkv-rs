//! `MemoryFile` equivalent: a page-aligned, resizable mapped file.

use std::io;
use std::path::{Path, PathBuf};

use mmkv_platform::{LockMode, LockOutcome, MappedRegion, Platform, SyncMode, initial_capacity};

pub struct MemoryFile {
    region: Box<dyn MappedRegion>,
    read_only: bool,
    page_size: usize,
    path: PathBuf,
}

impl MemoryFile {
    pub fn open(
        platform: &dyn Platform,
        path: &Path,
        expected_capacity: usize,
        read_only: bool,
        mayfly_fd: bool,
    ) -> io::Result<Self> {
        let page_size = platform.page_size();
        let expected = initial_capacity(expected_capacity, page_size);
        let region = platform.open_region(path, expected, read_only, mayfly_fd)?;
        Ok(Self {
            region,
            read_only,
            page_size,
            path: path.to_path_buf(),
        })
    }

    pub fn size(&self) -> usize {
        self.region.size()
    }

    /// Size of the file on disk, which may differ from the mapping in
    /// multi-process mode.
    pub fn actual_file_size(&self) -> io::Result<usize> {
        Ok(std::fs::metadata(&self.path)?.len() as usize)
    }

    pub fn bytes(&self) -> &[u8] {
        self.region.as_slice()
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        self.region.as_mut_slice()
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn resize(&mut self, new_size: usize) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cannot resize a read-only MMKV file",
            ));
        }
        let target = mmkv_platform::round_up_to_page(new_size, self.page_size).max(self.page_size);
        self.region.resize(target)
    }

    pub fn sync(&self, mode: SyncMode) -> io::Result<()> {
        self.region.sync(mode)
    }

    pub fn lock(&self, mode: LockMode, wait: bool) -> io::Result<LockOutcome> {
        self.region.lock(mode, wait)
    }

    pub fn unlock(&self, mode: LockMode) -> io::Result<()> {
        self.region.unlock(mode)
    }

    /// Duplicate the backing descriptor. `flock` locks are associated with the
    /// open file description, so a `dup` shares the lock — exactly the
    /// behaviour MMKV relies on when it locks the `.crc` mapping.
    pub fn try_clone_file(&self) -> Option<io::Result<std::fs::File>> {
        self.region.file().map(|file| file.try_clone())
    }
}
