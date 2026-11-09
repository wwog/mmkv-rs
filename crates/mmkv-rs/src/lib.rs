//! Public façade for the MMKV Rust port.
//!
//! Platform selection happens here, at compile time, like GPUI picks
//! `gpui_macos`/`gpui_linux`:
//!
//! * macOS → `mmkv-platform-macos` (Foundation access via `objc2`)
//! * other targets → `mmkv-platform-posix` until their dedicated crates land
//!
//! Everything else is re-exported from `mmkv-core`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mmkv_platform::Platform;

pub use mmkv_core::{
    CorruptKind, CryptKey, Error, Expiration, Handler, Level, Mmkv, Mode, Namespace, NoopHandler,
    Recover, Result, Store, StoreConfig, StoreLockGuard, VERSION, codec, config, crc32, error,
    handler, kv, log, memory, meta, namespace, path, registry, store,
};

#[cfg(target_os = "macos")]
pub type PlatformImpl = mmkv_platform_macos::MacPlatform;

#[cfg(not(target_os = "macos"))]
pub type PlatformImpl = mmkv_platform_posix::PosixPlatform;

/// Construct the platform integration for the current target.
#[cfg(target_os = "macos")]
pub fn default_platform() -> Arc<dyn Platform> {
    Arc::new(PlatformImpl::new())
}

#[cfg(not(target_os = "macos"))]
pub fn default_platform() -> Arc<dyn Platform> {
    Arc::new(PlatformImpl::new(None))
}

#[derive(Clone)]
pub struct InitOptions<'a> {
    /// Explicit root directory. When `None`, the platform decides
    /// (`NSDocumentDirectory/mmkv` on Apple platforms).
    pub root_dir: Option<&'a Path>,
    /// MMKV-compatible group directory (a path, not an App Group identifier).
    /// The root becomes `<group_dir>/mmkv`.
    pub group_dir: Option<&'a Path>,
    pub log_level: Level,
    pub handler: Option<Arc<dyn Handler>>,
}

impl Default for InitOptions<'_> {
    fn default() -> Self {
        Self {
            root_dir: None,
            group_dir: None,
            log_level: Level::Info,
            handler: None,
        }
    }
}

/// Initialize the global MMKV root. Subsequent calls keep the first root, like
/// `MMKV::initializeMMKV`.
pub fn init(options: InitOptions<'_>) -> Result<PathBuf> {
    let platform = default_platform();
    let root = match (options.root_dir, options.group_dir) {
        (Some(root), _) => Some(root.to_path_buf()),
        (None, Some(group)) => Some(group.join("mmkv")),
        (None, None) => None,
    };
    mmkv_core::registry::init(platform, root, options.log_level, options.handler)
}

pub fn is_initialized() -> bool {
    mmkv_core::registry::is_initialized()
}

pub fn root_dir() -> Option<PathBuf> {
    mmkv_core::registry::root_dir()
}

pub fn page_size() -> usize {
    mmkv_core::registry::platform()
        .map(|platform| platform.page_size())
        .unwrap_or_else(|| {
            #[cfg(target_os = "macos")]
            {
                PlatformImpl::new().page_size()
            }
            #[cfg(not(target_os = "macos"))]
            {
                mmkv_platform_posix::page_size()
            }
        })
}

pub fn open(id: impl Into<String>, config: &StoreConfig<'_>) -> Result<Mmkv> {
    mmkv_core::registry::open(id, config)
}

pub fn default_store(config: &StoreConfig<'_>) -> Result<Mmkv> {
    mmkv_core::registry::default_store(config)
}

pub fn namespace(root: impl AsRef<Path>) -> Result<Namespace> {
    let platform = mmkv_core::registry::platform().ok_or(Error::NotInitialized)?;
    Ok(Namespace::new(platform, root))
}

pub fn default_namespace() -> Result<Namespace> {
    let platform = mmkv_core::registry::platform().ok_or(Error::NotInitialized)?;
    let root = mmkv_core::registry::default_root_dir()?;
    Ok(Namespace::new(platform, root))
}

pub fn set_log_level(level: Level) {
    mmkv_core::log::set_level(level);
}

pub fn set_handler(handler: Option<Arc<dyn Handler>>) {
    mmkv_core::log::set_handler(handler);
}

pub fn shutdown() {
    mmkv_core::registry::shutdown();
}

fn resolve_root(root: Option<&Path>) -> Result<PathBuf> {
    match root {
        Some(root) => Ok(mmkv_core::path::absolute_path(root)),
        None => mmkv_core::registry::default_root_dir(),
    }
}

pub fn is_file_valid(id: &str, root: Option<&Path>) -> bool {
    resolve_root(root)
        .map(|root| Store::is_file_valid(id, &root))
        .unwrap_or(false)
}

pub fn exists(id: &str, root: Option<&Path>) -> bool {
    resolve_root(root)
        .map(|root| Store::exists(id, &root))
        .unwrap_or(false)
}

pub fn remove_storage(id: &str, root: Option<&Path>) -> Result<bool> {
    let root = resolve_root(root)?;
    Store::remove_storage(id, &root)
}

pub fn backup_one(id: &str, dst_dir: &Path, src_root: Option<&Path>) -> bool {
    resolve_root(src_root)
        .map(|root| Store::backup_one(id, dst_dir, &root))
        .unwrap_or(false)
}

pub fn restore_one(id: &str, src_dir: &Path, dst_root: Option<&Path>) -> bool {
    resolve_root(dst_root)
        .map(|root| Store::restore_one(id, src_dir, &root))
        .unwrap_or(false)
}

pub fn backup_all(dst_dir: &Path, src_root: Option<&Path>) -> usize {
    resolve_root(src_root)
        .map(|root| Store::backup_all(dst_dir, &root))
        .unwrap_or(0)
}

pub fn restore_all(src_dir: &Path, dst_root: Option<&Path>) -> usize {
    resolve_root(dst_root)
        .map(|root| Store::restore_all(src_dir, &root))
        .unwrap_or(0)
}
