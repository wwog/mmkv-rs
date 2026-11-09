//! macOS platform integration.
//!
//! Foundation is reached through `objc2`/`objc2-foundation`; there is no Swift
//! shim and no Objective-C++ build step. The paths mirror `+[MMKV mmkvBasePath]`:
//!
//! * macOS/iOS/watchOS/visionOS → `NSDocumentDirectory/mmkv`
//! * tvOS → `NSCachesDirectory/mmkv`
//! * App Group → `<group container>/mmkv`

use std::io;
use std::path::{Path, PathBuf};

use mmkv_platform::{AppState, MappedRegion, Platform};
use mmkv_platform_posix::PosixMappedRegion;
use objc2_foundation::{
    NSArray, NSBundle, NSFileManager, NSSearchPathDirectory, NSSearchPathDomainMask,
    NSSearchPathForDirectoriesInDomains, NSString, NSTemporaryDirectory, NSURL,
};

fn ns_string(path: &Path) -> io::Result<objc2::rc::Retained<NSString>> {
    let text = path
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path is not valid UTF-8"))?;
    Ok(NSString::from_str(text))
}

fn path_from_ns(value: &NSString) -> PathBuf {
    PathBuf::from(value.to_string())
}

/// Search-path directory used by MMKV on this OS family.
#[cfg(target_os = "tvos")]
const BASE_DIRECTORY: NSSearchPathDirectory = NSSearchPathDirectory::CachesDirectory;
#[cfg(not(target_os = "tvos"))]
const BASE_DIRECTORY: NSSearchPathDirectory = NSSearchPathDirectory::DocumentDirectory;

/// `<base>/mmkv`, exactly like `+[MMKV mmkvBasePath]`.
fn base_path() -> io::Result<PathBuf> {
    let paths: objc2::rc::Retained<NSArray<NSString>> = NSSearchPathForDirectoriesInDomains(
        BASE_DIRECTORY,
        NSSearchPathDomainMask::UserDomainMask,
        true,
    );
    let base = paths
        .firstObject()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no NSDocumentDirectory"))?;
    Ok(path_from_ns(&base).join("mmkv"))
}

/// macOS implementation of the MMKV platform contract.
#[derive(Debug, Clone, Copy)]
pub struct MacPlatform {
    page_size: usize,
}

impl Default for MacPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl MacPlatform {
    pub fn new() -> Self {
        Self {
            page_size: mmkv_platform_posix::page_size(),
        }
    }
}

impl Platform for MacPlatform {
    fn name(&self) -> &'static str {
        "macos"
    }

    fn default_root_dir(&self) -> io::Result<PathBuf> {
        base_path()
    }

    fn shared_root_dir(&self, group: Option<&str>) -> io::Result<PathBuf> {
        match group {
            None => self.default_root_dir(),
            Some(group) => {
                let identifier = NSString::from_str(group);
                let url: Option<objc2::rc::Retained<NSURL>> = NSFileManager::defaultManager()
                    .containerURLForSecurityApplicationGroupIdentifier(&identifier);
                let url = url.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("no App Group container for {group:?}"),
                    )
                })?;
                let path = url.path().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "App Group URL has no path")
                })?;
                Ok(path_from_ns(&path).join("mmkv"))
            }
        }
    }

    fn temp_dir(&self) -> io::Result<PathBuf> {
        Ok(path_from_ns(&NSTemporaryDirectory()))
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
        mmkv_platform_posix::open_region(path, expected_size, read_only, mayfly_fd, self.page_size)
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        let path = ns_string(path)?;
        let manager = NSFileManager::defaultManager();
        // SAFETY: no attributes are passed; the method only touches the path.
        unsafe {
            manager.createDirectoryAtPath_withIntermediateDirectories_attributes_error(
                &path, true, None,
            )
        }
        .map_err(|error| io::Error::other(error.to_string()))
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

/// Whether the current process runs inside an `.appex` bundle.
pub fn is_app_extension() -> bool {
    NSBundle::mainBundle()
        .bundlePath()
        .to_string()
        .ends_with(".appex")
}

/// Re-export for callers that only need the generic POSIX region type.
pub type MappedFile = PosixMappedRegion;
