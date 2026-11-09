#![cfg(target_os = "ios")]
//! iOS platform integration.
//!
//! Uses `objc2`/`objc2-foundation` directly (no Swift shim):
//! * root: `NSDocumentDirectory/mmkv` (MMKV's `+[MMKV mmkvBasePath]`)
//! * App Group: `<container>/mmkv`
//! * `NSFileProtectionCompleteUntilFirstUserAuthentication` on writable files
//! * background/active/memory-warning notifications feed `AppLifecycle`

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(target_os = "ios")]
use block2::RcBlock;
use mmkv_platform::{AppEvent, AppState, LockMode, LockOutcome, MappedRegion, Platform};
use objc2::rc::Retained;
#[cfg(target_os = "ios")]
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2_foundation::{
    NSArray, NSBundle, NSFileManager, NSSearchPathDirectory, NSSearchPathDomainMask,
    NSSearchPathForDirectoriesInDomains, NSString, NSTemporaryDirectory, NSURL,
};
#[cfg(target_os = "ios")]
use objc2_foundation::{
    NSDictionary, NSFileAttributeKey, NSFileProtectionCompleteUntilFirstUserAuthentication,
    NSFileProtectionKey, NSNotification, NSNotificationCenter, NSObjectProtocol,
};
#[cfg(target_os = "ios")]
use std::ptr::NonNull;

fn ns_string(value: &str) -> Retained<NSString> {
    NSString::from_str(value)
}

fn path_from_ns(value: &NSString) -> PathBuf {
    PathBuf::from(value.to_string())
}

fn ns_path(path: &Path) -> io::Result<Retained<NSString>> {
    let text = path
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path is not valid UTF-8"))?;
    Ok(ns_string(text))
}

fn base_path() -> io::Result<PathBuf> {
    let paths: Retained<NSArray<NSString>> = NSSearchPathForDirectoriesInDomains(
        NSSearchPathDirectory::DocumentDirectory,
        NSSearchPathDomainMask::UserDomainMask,
        true,
    );
    let base = paths
        .firstObject()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no NSDocumentDirectory"))?;
    Ok(path_from_ns(&base).join("mmkv"))
}

fn container_path(group: &str) -> io::Result<PathBuf> {
    let identifier = ns_string(group);
    let url: Option<Retained<NSURL>> = NSFileManager::defaultManager()
        .containerURLForSecurityApplicationGroupIdentifier(&identifier);
    let url = url.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("no App Group container for {group:?}"),
        )
    })?;
    let path = url
        .path()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "App Group URL has no path"))?;
    Ok(path_from_ns(&path).join("mmkv"))
}

/// Shared lifecycle state; the notifications update `background`.
#[derive(Debug)]
pub struct AppleLifecycle {
    background: AtomicBool,
}

impl Default for AppleLifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl AppleLifecycle {
    pub fn new() -> Self {
        Self {
            background: AtomicBool::new(false),
        }
    }

    pub fn is_background(&self) -> bool {
        self.background.load(Ordering::Acquire)
    }
}

/// iOS implementation of the MMKV platform contract.
#[derive(Clone)]
pub struct IosPlatform {
    page_size: usize,
    lifecycle: Arc<AppleLifecycle>,
}

impl Default for IosPlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl IosPlatform {
    pub fn new() -> Self {
        let lifecycle = AppleLifecycle::new();
        #[cfg(target_os = "ios")]
        if !is_app_extension() {
            // Match MMKV's initial UIApplicationState check without importing
            // UIKit: `UIApplicationState.background == 2`.
            use objc2::runtime::AnyClass;
            let background = unsafe {
                AnyClass::get(c"UIApplication")
                    .and_then(|class| {
                        let application: *mut objc2::runtime::AnyObject =
                            objc2::msg_send![class, sharedApplication];
                        if application.is_null() {
                            None
                        } else {
                            let state: isize = objc2::msg_send![application, applicationState];
                            Some(state == 2)
                        }
                    })
                    .unwrap_or(false)
            };
            lifecycle.background.store(background, Ordering::Release);
        }
        Self {
            page_size: mmkv_platform_posix::page_size(),
            lifecycle: Arc::new(lifecycle),
        }
    }

    pub fn lifecycle(&self) -> &AppleLifecycle {
        &self.lifecycle
    }
}

impl Platform for IosPlatform {
    fn name(&self) -> &'static str {
        "ios"
    }

    fn default_root_dir(&self) -> io::Result<PathBuf> {
        base_path()
    }

    fn shared_root_dir(&self, group: Option<&str>) -> io::Result<PathBuf> {
        match group {
            None => self.default_root_dir(),
            Some(group) => container_path(group),
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
        let path = ns_path(path)?;
        let manager = NSFileManager::defaultManager();
        // SAFETY: no attributes are passed.
        unsafe {
            manager.createDirectoryAtPath_withIntermediateDirectories_attributes_error(
                &path, true, None,
            )
        }
        .map_err(|error| io::Error::other(error.to_string()))
    }

    fn protect_file(&self, path: &Path) -> io::Result<()> {
        #[cfg(target_os = "ios")]
        {
            let path = ns_path(path)?;
            let manager = NSFileManager::defaultManager();
            // SAFETY: Foundation exposes these as immutable extern statics.
            let protection_key = unsafe { NSFileProtectionKey };
            let protection_value = unsafe { NSFileProtectionCompleteUntilFirstUserAuthentication };
            let attributes: Retained<NSDictionary<NSFileAttributeKey, NSString>> =
                NSDictionary::from_slices(&[protection_key], &[protection_value]);
            // The generated method takes `NSDictionary<NSFileAttributeKey, AnyObject>`;
            // the dictionary is layout-identical (values are NSStrings).
            let attributes: &NSDictionary<NSFileAttributeKey, AnyObject> = unsafe {
                &*(&*attributes as *const NSDictionary<NSFileAttributeKey, NSString>
                    as *const NSDictionary<NSFileAttributeKey, AnyObject>)
            };
            // SAFETY: attribute dictionary and path are valid.
            unsafe { manager.setAttributes_ofItemAtPath_error(attributes, &path) }
                .map_err(|error| io::Error::other(error.to_string()))?;
            Ok(())
        }
        #[cfg(not(target_os = "ios"))]
        {
            let _ = path;
            Ok(())
        }
    }

    fn lock_file(
        &self,
        file: &std::fs::File,
        mode: LockMode,
        wait: bool,
    ) -> io::Result<LockOutcome> {
        mmkv_platform_posix::lock_file(file, mode, wait)
    }

    fn unlock_file(&self, file: &std::fs::File, mode: LockMode) -> io::Result<()> {
        mmkv_platform_posix::unlock_file(file, mode)
    }

    fn random_bytes(&self, out: &mut [u8]) -> io::Result<()> {
        mmkv_platform_posix::random_bytes(out)
    }

    fn now_unix_seconds(&self) -> u32 {
        mmkv_platform_posix::now_unix_seconds()
    }

    fn app_state(&self) -> AppState {
        if self.lifecycle.is_background() {
            AppState::Background
        } else {
            AppState::Foreground
        }
    }

    fn subscribe_app_events(&self, cb: Box<dyn Fn(AppEvent) + Send + Sync>) -> Box<dyn FnOnce()> {
        #[cfg(not(target_os = "ios"))]
        {
            let _ = cb;
            Box::new(|| {})
        }

        #[cfg(target_os = "ios")]
        {
            let callback = Arc::new(cb);
            let background = self.lifecycle.clone();
            let center = NSNotificationCenter::defaultCenter();

            let mut tokens: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>> = Vec::new();
            let mut observers: Vec<RcBlock<dyn Fn(NonNull<NSNotification>)>> = Vec::new();

            let mut subscribe = |name: &str, event: AppEvent| {
                let notification_name = ns_string(name);
                let callback = callback.clone();
                let background = background.clone();
                let block = RcBlock::new(move |_notification: NonNull<NSNotification>| {
                    match event {
                        AppEvent::EnterBackground => {
                            background.background.store(true, Ordering::Release)
                        }
                        AppEvent::BecameActive => {
                            background.background.store(false, Ordering::Release)
                        }
                        AppEvent::MemoryWarning => {}
                    }
                    callback(event);
                });
                // SAFETY: the notification name is valid and the block is
                // copied by NSNotificationCenter.
                let token = unsafe {
                    center.addObserverForName_object_queue_usingBlock(
                        Some(&notification_name),
                        None,
                        None,
                        &block,
                    )
                };
                tokens.push(token);
                observers.push(block);
            };

            subscribe(
                "UIApplicationDidEnterBackgroundNotification",
                AppEvent::EnterBackground,
            );
            subscribe(
                "UIApplicationDidBecomeActiveNotification",
                AppEvent::BecameActive,
            );
            subscribe(
                "UIApplicationDidReceiveMemoryWarningNotification",
                AppEvent::MemoryWarning,
            );

            Box::new(move || {
                let center = NSNotificationCenter::defaultCenter();
                for token in &tokens {
                    // SAFETY: the token came from this center; it is an
                    // NSObject and can be passed as AnyObject.
                    let observer: &AnyObject = unsafe {
                        &*(&**token as *const ProtocolObject<dyn NSObjectProtocol>
                            as *const AnyObject)
                    };
                    let _: () = unsafe { objc2::msg_send![&center, removeObserver: observer] };
                }
                drop(observers);
            })
        }
    }
}

/// Whether the process runs inside an `.appex` bundle.
pub fn is_app_extension() -> bool {
    NSBundle::mainBundle()
        .bundlePath()
        .to_string()
        .ends_with(".appex")
}

/// Convenience: the platform's current background flag.
pub fn is_in_background(platform: &IosPlatform) -> bool {
    platform.lifecycle.is_background()
}
