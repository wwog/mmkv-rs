//! Public façade for the MMKV Rust port.
//!
//! Platform selection happens here, at compile time, like GPUI picks
//! `gpui_macos`/`gpui_linux`:
//!
//! * macOS / tvOS / watchOS / visionOS → `mmkv-platform-macos` via `objc2`
//! * iOS → `mmkv-platform-ios` (`objc2`, file protection, lifecycle)
//! * Linux → `mmkv-platform-linux`
//! * Android → `mmkv-platform-android` (JNI paths, ashmem, mode check)
//! * Windows → `mmkv-platform-windows` (`CreateFileMappingW`, `LockFileEx`)
//! * anything else → `mmkv-platform-posix`
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

#[cfg(target_os = "android")]
pub use mmkv_platform_android as android;
#[cfg(target_os = "ios")]
pub use mmkv_platform_ios as ios;
#[cfg(target_os = "linux")]
pub use mmkv_platform_linux as linux;
#[cfg(any(
    target_os = "macos",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "visionos"
))]
pub use mmkv_platform_macos as apple;
#[cfg(target_os = "windows")]
pub use mmkv_platform_windows as windows;

#[cfg(any(
    target_os = "macos",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "visionos"
))]
pub type PlatformImpl = mmkv_platform_macos::MacPlatform;

#[cfg(target_os = "ios")]
pub type PlatformImpl = mmkv_platform_ios::IosPlatform;

#[cfg(target_os = "linux")]
pub type PlatformImpl = mmkv_platform_linux::LinuxPlatform;

#[cfg(target_os = "android")]
pub type PlatformImpl = mmkv_platform_android::AndroidPlatform;

#[cfg(target_os = "windows")]
pub type PlatformImpl = mmkv_platform_windows::WindowsPlatform;

#[cfg(not(any(
    target_os = "macos",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "visionos",
    target_os = "ios",
    target_os = "linux",
    target_os = "android",
    target_os = "windows"
)))]
pub type PlatformImpl = mmkv_platform_posix::PosixPlatform;

/// Construct the platform integration for the current target.
#[cfg(any(
    target_os = "macos",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "visionos"
))]
pub fn default_platform() -> Arc<dyn Platform> {
    Arc::new(PlatformImpl::new())
}

#[cfg(target_os = "ios")]
pub fn default_platform() -> Arc<dyn Platform> {
    Arc::new(PlatformImpl::new())
}

#[cfg(target_os = "linux")]
pub fn default_platform() -> Arc<dyn Platform> {
    Arc::new(PlatformImpl::new(None))
}

#[cfg(target_os = "android")]
pub fn default_platform() -> Arc<dyn Platform> {
    // Apps should prefer `init_with(AndroidPlatform::from_java(...))` or pass
    // `InitOptions::root_dir`; this keeps construction infallible.
    Arc::new(PlatformImpl::from_env())
}

#[cfg(target_os = "windows")]
pub fn default_platform() -> Arc<dyn Platform> {
    Arc::new(PlatformImpl::new(None))
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "tvos",
    target_os = "watchos",
    target_os = "visionos",
    target_os = "ios",
    target_os = "linux",
    target_os = "android",
    target_os = "windows"
)))]
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
    init_with(default_platform(), options)
}

/// Initialize with an explicit platform. Useful on Android (JNI-resolved
/// paths) and for embedding/testing custom platforms.
pub fn init_with(platform: Arc<dyn Platform>, options: InitOptions<'_>) -> Result<PathBuf> {
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
        .unwrap_or_else(|| default_platform().page_size())
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

/// Close instances idle for longer than `max_idle_minutes`.
pub fn enable_auto_cleanup(max_idle_minutes: u32) {
    mmkv_core::registry::enable_auto_cleanup(max_idle_minutes);
}

pub fn disable_auto_cleanup() {
    mmkv_core::registry::disable_auto_cleanup();
}

pub fn try_auto_cleanup_instances() {
    mmkv_core::registry::try_auto_cleanup_instances();
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

fn operation_platform() -> Arc<dyn Platform> {
    mmkv_core::registry::platform().unwrap_or_else(default_platform)
}

pub fn remove_storage(id: &str, root: Option<&Path>) -> Result<bool> {
    let root = resolve_root(root)?;
    let platform = operation_platform();
    Store::remove_storage(id, &root, platform.as_ref())
}

pub fn backup_one(id: &str, dst_dir: &Path, src_root: Option<&Path>) -> bool {
    let platform = operation_platform();
    resolve_root(src_root)
        .map(|root| Store::backup_one(id, dst_dir, &root, platform.as_ref()))
        .unwrap_or(false)
}

pub fn restore_one(id: &str, src_dir: &Path, dst_root: Option<&Path>) -> bool {
    let platform = operation_platform();
    resolve_root(dst_root)
        .map(|root| Store::restore_one(id, src_dir, &root, platform.as_ref()))
        .unwrap_or(false)
}

pub fn backup_all(dst_dir: &Path, src_root: Option<&Path>) -> usize {
    let platform = operation_platform();
    resolve_root(src_root)
        .map(|root| Store::backup_all(dst_dir, &root, platform.as_ref()))
        .unwrap_or(0)
}

pub fn restore_all(src_dir: &Path, dst_root: Option<&Path>) -> usize {
    let platform = operation_platform();
    resolve_root(dst_root)
        .map(|root| Store::restore_all(src_dir, &root, platform.as_ref()))
        .unwrap_or(0)
}
