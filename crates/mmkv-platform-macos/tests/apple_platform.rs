#![cfg(target_vendor = "apple")]

//! Verifies the `objc2` implementation exposes the same Foundation paths and
//! behaviour MMKV uses on Apple platforms.

use mmkv_platform::Platform;
use mmkv_platform_macos::MacPlatform;

#[test]
fn default_root_is_documents_mmkv() {
    let platform = MacPlatform::new();
    let root = platform.default_root_dir().expect("Foundation root");
    let text = root.to_string_lossy();
    assert!(
        text.ends_with("/Documents/mmkv"),
        "unexpected MMKV root: {text}"
    );
}

#[test]
fn page_size_is_a_sane_page_multiple() {
    let platform = MacPlatform::new();
    let page = platform.page_size();
    assert!(page >= 4096, "page size {page}");
    assert!(page.is_power_of_two(), "page size {page}");
}

#[test]
fn create_dir_all_goes_through_nsfilemanager() {
    let platform = MacPlatform::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/mmkv");
    platform.create_dir_all(&path).unwrap();
    assert!(path.is_dir());
}

#[test]
fn file_protection_is_a_noop_on_macos() {
    let platform = MacPlatform::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    std::fs::write(&path, b"x").unwrap();
    platform.protect_file(&path).unwrap();
}

#[test]
fn app_group_resolution_is_graceful() {
    let platform = MacPlatform::new();
    // Foundation may synthesize a container URL even without an entitlement,
    // or return nil. Either way the shim must not panic across the FFI
    // boundary, and a successful path must end in the MMKV subdirectory.
    if let Ok(path) = platform.shared_root_dir(Some("group.mmkv.rs.does-not-exist")) {
        assert!(path.to_string_lossy().ends_with("/mmkv"));
    }
}

#[test]
fn process_is_not_an_app_extension() {
    assert!(!mmkv_platform_macos::is_app_extension());
}
