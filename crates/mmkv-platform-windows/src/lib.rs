#![cfg(windows)]
//! Windows platform: `CreateFileMappingW`/`MapViewOfFile` and `LockFileEx`.
//!
//! MMKV on Windows requires an explicit root directory, so `default_root_dir`
//! reports `Unsupported` unless one was provided.

#[cfg(windows)]
mod windows_impl {
    use std::fs::{File, OpenOptions};
    use std::io::{self, Seek, SeekFrom, Write};
    use std::os::windows::io::AsRawHandle;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use mmkv_platform::{
        AppState, LockMode, LockOutcome, MappedRegion, Platform, SyncMode, initial_capacity,
        round_up_to_page,
    };
    use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, GetLastError, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::{
        FlushFileBuffers, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
        UnlockFileEx,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, FlushViewOfFile, MapViewOfFile,
        PAGE_READONLY, PAGE_READWRITE, UnmapViewOfFile,
    };
    use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};

    fn handle_of(file: &File) -> HANDLE {
        file.as_raw_handle() as HANDLE
    }

    fn zero_fill(file: &mut File, start: u64, len: usize) -> io::Result<()> {
        const ZEROS: [u8; 4096] = [0; 4096];
        file.seek(SeekFrom::Start(start))?;
        let mut written = 0;
        while written < len {
            let chunk = (len - written).min(ZEROS.len());
            file.write_all(&ZEROS[..chunk])?;
            written += chunk;
        }
        Ok(())
    }

    #[derive(Clone, Copy)]
    enum View {
        Empty,
        Mapped { address: usize, len: usize },
    }

    pub struct WindowsMappedRegion {
        file: File,
        view: View,
        read_only: bool,
        page_size: usize,
    }

    impl WindowsMappedRegion {
        pub fn open(
            path: &Path,
            expected_size: usize,
            read_only: bool,
            mayfly_fd: bool,
        ) -> io::Result<Self> {
            let page_size = page_size();
            let mut file = if read_only {
                OpenOptions::new().read(true).open(path)?
            } else {
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(path)?
            };
            let actual = file.metadata()?.len() as usize;
            let mut target = if read_only {
                // Read-only mappings use the real file size, never a
                // page-rounded over-map.
                actual
            } else {
                initial_capacity(expected_size, page_size)
            };
            if !read_only {
                if actual > 0 {
                    if !actual.is_multiple_of(page_size) {
                        target = round_up_to_page(actual, page_size);
                    } else if actual >= page_size {
                        target = actual;
                    }
                }
                if actual != target {
                    if actual > target {
                        target = actual;
                    }
                    file.set_len(target as u64)?;
                    if target > actual {
                        zero_fill(&mut file, actual as u64, target - actual)?;
                    }
                }
            }
            let mut region = Self {
                file,
                view: View::Empty,
                read_only,
                page_size,
            };
            region.map(target)?;
            // The view keeps the section alive on Windows; the file handle is
            // kept so LockFileEx remains usable.
            let _ = mayfly_fd;
            Ok(region)
        }

        fn map(&mut self, len: usize) -> io::Result<()> {
            if len == 0 {
                self.view = View::Empty;
                return Ok(());
            }
            let protect = if self.read_only {
                PAGE_READONLY
            } else {
                PAGE_READWRITE
            };
            let access = if self.read_only {
                FILE_MAP_READ
            } else {
                FILE_MAP_READ | FILE_MAP_WRITE
            };
            // SAFETY: valid file handle; named mapping not used.
            let mapping = unsafe {
                CreateFileMappingW(
                    handle_of(&self.file),
                    std::ptr::null(),
                    protect,
                    0,
                    0,
                    std::ptr::null(),
                )
            };
            if mapping.is_null() {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: mapping is valid; map from offset 0.
            let address = unsafe { MapViewOfFile(mapping, access, 0, 0, len) };
            // The view owns a reference to the section.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(mapping) };
            if address.Value.is_null() {
                return Err(io::Error::last_os_error());
            }
            self.view = View::Mapped {
                address: address.Value as usize,
                len,
            };
            Ok(())
        }

        fn unmap(&mut self) {
            if let View::Mapped { address, .. } = self.view {
                // SAFETY: the view was created by MapViewOfFile.
                unsafe {
                    UnmapViewOfFile(
                        windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                            Value: address as *mut std::ffi::c_void,
                        },
                    )
                };
            }
            self.view = View::Empty;
        }

        fn len(&self) -> usize {
            match self.view {
                View::Empty => 0,
                View::Mapped { len, .. } => len,
            }
        }

        fn ptr(&self) -> *mut u8 {
            match self.view {
                View::Empty => std::ptr::null_mut(),
                View::Mapped { address, .. } => address as *mut u8,
            }
        }
    }

    impl MappedRegion for WindowsMappedRegion {
        fn size(&self) -> usize {
            self.len()
        }

        fn as_slice(&self) -> &[u8] {
            let len = self.len();
            if len == 0 {
                return &[];
            }
            // SAFETY: mapped region is valid for `len` bytes.
            unsafe { std::slice::from_raw_parts(self.ptr().cast_const(), len) }
        }

        fn as_mut_slice(&mut self) -> &mut [u8] {
            let len = self.len();
            if len == 0 {
                return &mut [];
            }
            // SAFETY: writable mapping is valid for `len` bytes.
            unsafe { std::slice::from_raw_parts_mut(self.ptr(), len) }
        }

        fn resize(&mut self, new_size: usize) -> io::Result<()> {
            if self.read_only {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "cannot resize a read-only MMKV region",
                ));
            }
            let target = round_up_to_page(new_size, self.page_size).max(self.page_size);
            let old = self.len();
            if target == old {
                return Ok(());
            }
            // SetEndOfFile fails while a view of the file is still mapped
            // (ERROR_USER_MAPPED_FILE); C++ MMKV unmaps first for the same
            // reason. Restore the old view if resizing fails.
            self.unmap();
            if let Err(err) = self.file.set_len(target as u64) {
                let _ = self.map(old);
                return Err(err);
            }
            if target > old
                && let Err(err) = zero_fill(&mut self.file, old as u64, target - old)
            {
                let _ = self.file.set_len(old as u64);
                let _ = self.map(old);
                return Err(err);
            }
            self.map(target)
        }

        fn sync(&self, mode: SyncMode) -> io::Result<()> {
            if self.read_only || self.len() == 0 {
                return Ok(());
            }
            // SAFETY: valid view/handle.
            unsafe {
                FlushViewOfFile(
                    self.ptr().cast::<std::ffi::c_void>().cast_const(),
                    self.len(),
                );
            }
            if matches!(mode, SyncMode::Sync) {
                // SAFETY: valid file handle.
                unsafe { FlushFileBuffers(handle_of(&self.file)) };
            }
            Ok(())
        }

        fn lock(&self, mode: LockMode, wait: bool) -> io::Result<LockOutcome> {
            lock_handle(handle_of(&self.file), mode, wait)
        }

        fn unlock(&self, mode: LockMode) -> io::Result<()> {
            unlock_handle(handle_of(&self.file), mode)
        }

        fn file(&self) -> Option<&File> {
            Some(&self.file)
        }
    }

    fn lock_handle(handle: HANDLE, mode: LockMode, wait: bool) -> io::Result<LockOutcome> {
        let mut flags = 0u32;
        if matches!(mode, LockMode::Exclusive) {
            flags |= LOCKFILE_EXCLUSIVE_LOCK;
        }
        if !wait {
            flags |= LOCKFILE_FAIL_IMMEDIATELY;
        }
        let mut overlapped = OVERLAPPED::default();
        // SAFETY: valid handle and overlapped structure.
        let ok = unsafe { LockFileEx(handle, flags, 0, u32::MAX, u32::MAX, &mut overlapped) };
        if ok != 0 {
            return Ok(LockOutcome::Acquired);
        }
        let error = unsafe { GetLastError() };
        if error == ERROR_LOCK_VIOLATION {
            Ok(LockOutcome::WouldBlock)
        } else {
            Err(io::Error::from_raw_os_error(error as i32))
        }
    }

    fn unlock_handle(handle: HANDLE, _mode: LockMode) -> io::Result<()> {
        let mut overlapped = OVERLAPPED::default();
        // SAFETY: valid handle and overlapped structure.
        let ok = unsafe { UnlockFileEx(handle, 0, u32::MAX, u32::MAX, &mut overlapped) };
        if ok != 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn page_size() -> usize {
        let mut info = SYSTEM_INFO::default();
        // SAFETY: valid out pointer.
        unsafe { GetSystemInfo(&mut info) };
        if info.dwPageSize == 0 {
            4096
        } else {
            info.dwPageSize as usize
        }
    }

    #[derive(Debug, Clone)]
    pub struct WindowsPlatform {
        root: Option<PathBuf>,
        page_size: usize,
    }

    impl WindowsPlatform {
        pub fn new(root: Option<PathBuf>) -> Self {
            Self {
                root,
                page_size: page_size(),
            }
        }

        pub fn with_root(root: impl Into<PathBuf>) -> Self {
            Self::new(Some(root.into()))
        }
    }

    impl Platform for WindowsPlatform {
        fn name(&self) -> &'static str {
            "windows"
        }

        fn default_root_dir(&self) -> io::Result<PathBuf> {
            self.root.clone().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "MMKV on Windows requires an explicit root directory",
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
            self.page_size
        }

        fn open_region(
            &self,
            path: &Path,
            expected_size: usize,
            read_only: bool,
            mayfly_fd: bool,
        ) -> io::Result<Box<dyn MappedRegion>> {
            Ok(Box::new(WindowsMappedRegion::open(
                path,
                expected_size,
                read_only,
                mayfly_fd,
            )?))
        }

        fn create_dir_all(&self, path: &Path) -> io::Result<()> {
            std::fs::create_dir_all(path)
        }

        fn lock_file(&self, file: &File, mode: LockMode, wait: bool) -> io::Result<LockOutcome> {
            lock_handle(handle_of(file), mode, wait)
        }

        fn unlock_file(&self, file: &File, mode: LockMode) -> io::Result<()> {
            unlock_handle(handle_of(file), mode)
        }

        fn random_bytes(&self, out: &mut [u8]) -> io::Result<()> {
            getrandom::fill(out).map_err(|err| io::Error::other(err.to_string()))
        }

        fn now_unix_seconds(&self) -> u32 {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs().min(u32::MAX as u64) as u32)
                .unwrap_or(0)
        }

        fn app_state(&self) -> AppState {
            AppState::Foreground
        }
    }
}

#[cfg(windows)]
pub use windows_impl::WindowsPlatform;
