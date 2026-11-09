//! Plaintext store round-trips without the C++ reference implementation.
//! These tests exercise the append log, full write-back, growth, deletion and
//! reopening paths.

use std::sync::Arc;

use mmkv_core::{Mode, Store, StoreConfig};
use mmkv_platform_macos::MacPlatform;

fn open(root: &std::path::Path, id: &str, mode: Mode) -> Arc<Store> {
    let platform = Arc::new(MacPlatform::new());
    Store::open_at(platform, root, id, &StoreConfig::default().mode(mode)).unwrap()
}

#[test]
fn primitive_and_string_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path(), "roundtrip", Mode::SINGLE_PROCESS);

    store.put_bool("bool", true).unwrap();
    store.put_i32("i32", -42).unwrap();
    store.put_u32("u32", 3_000_000_000).unwrap();
    store.put_i64("i64", -9_000_000_000).unwrap();
    store.put_u64("u64", u64::MAX).unwrap();
    store.put_f32("f32", 3.5).unwrap();
    store.put_f64("f64", -2.25).unwrap();
    store.put_str("str", "hello 中文").unwrap();
    store.put_str("empty", "").unwrap();
    store.put_bytes("bytes", &[0, 1, 2, 255]).unwrap();
    store.put_bytes("empty_bytes", &[]).unwrap();
    store
        .put_strings("strings", &["a".to_owned(), String::new(), "b".to_owned()])
        .unwrap();
    store.put_i32_vec("i32_vec", &[-1, 0, 1, i32::MIN]).unwrap();
    store
        .put_f64_vec("f64_vec", &[1.5, -0.0, f64::INFINITY])
        .unwrap();

    assert!(store.get_bool("bool", false));
    assert_eq!(store.get_i32("i32", 0), -42);
    assert_eq!(store.get_u32("u32", 0), 3_000_000_000);
    assert_eq!(store.get_i64("i64", 0), -9_000_000_000);
    assert_eq!(store.get_u64("u64", 0), u64::MAX);
    assert_eq!(store.get_f32("f32", 0.0), 3.5);
    assert_eq!(store.get_f64("f64", 0.0), -2.25);
    assert_eq!(store.get_str("str").as_deref(), Some("hello 中文"));
    assert_eq!(store.get_str("empty").as_deref(), Some(""));
    assert_eq!(store.get_bytes("bytes"), Some(vec![0, 1, 2, 255]));
    assert_eq!(store.get_bytes("empty_bytes"), Some(Vec::new()));
    assert_eq!(
        store.get_strings("strings"),
        Some(vec!["a".to_owned(), String::new(), "b".to_owned()])
    );
    assert_eq!(store.get_i32_vec("i32_vec"), Some(vec![-1, 0, 1, i32::MIN]));
    assert_eq!(
        store.get_f64_vec("f64_vec"),
        Some(vec![1.5, -0.0, f64::INFINITY])
    );
    assert_eq!(store.get_str("missing"), None);
    assert_eq!(store.count(), 14);

    store.sync();
    drop(store);

    let reopened = open(dir.path(), "roundtrip", Mode::SINGLE_PROCESS);
    assert_eq!(reopened.count(), 14);
    assert_eq!(reopened.get_i32("i32", 0), -42);
    assert_eq!(reopened.get_str("str").as_deref(), Some("hello 中文"));
    assert!(Store::is_file_valid("roundtrip", dir.path()));
}

#[test]
fn many_keys_trigger_full_writeback_and_growth() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path(), "growth", Mode::SINGLE_PROCESS);

    let value = "x".repeat(200);
    for index in 0..2_000 {
        store.put_str(&format!("key-{index:04}"), &value).unwrap();
    }
    assert_eq!(store.count(), 2_000);
    assert_eq!(store.get_str("key-0000").as_deref(), Some(value.as_str()));
    assert_eq!(store.get_str("key-1999").as_deref(), Some(value.as_str()));
    assert!(store.total_size() > 2_000 * 200);

    store.sync();
    drop(store);

    let reopened = open(dir.path(), "growth", Mode::SINGLE_PROCESS);
    assert_eq!(reopened.count(), 2_000);
    assert_eq!(
        reopened.get_str("key-1234").as_deref(),
        Some(value.as_str())
    );
    assert!(Store::is_file_valid("growth", dir.path()));
}

#[test]
fn deletes_and_clear_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path(), "deletes", Mode::SINGLE_PROCESS);

    for index in 0..10 {
        store.put_i32(&format!("k{index}"), index).unwrap();
    }
    assert!(store.remove("k3").unwrap());
    assert!(!store.remove("nope").unwrap());
    assert_eq!(store.count(), 9);
    assert_eq!(store.get_i32("k3", -1), -1);

    store.sync();
    drop(store);

    let reopened = open(dir.path(), "deletes", Mode::SINGLE_PROCESS);
    assert_eq!(reopened.count(), 9);
    assert_eq!(reopened.get_i32("k3", -1), -1);
    assert_eq!(reopened.get_i32("k4", -1), 4);

    reopened.clear();
    assert_eq!(reopened.count(), 0);
    reopened.sync();
    drop(reopened);

    let empty = open(dir.path(), "deletes", Mode::SINGLE_PROCESS);
    assert_eq!(empty.count(), 0);
    assert_eq!(empty.get_i32("k4", -1), -1);
}

#[test]
fn trim_reclaims_space_and_keeps_data() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path(), "trim", Mode::SINGLE_PROCESS);

    for index in 0..500 {
        store
            .put_str(&format!("k{index}"), &"y".repeat(300))
            .unwrap();
    }
    let before = store.total_size();
    for index in 0..480 {
        store.remove(&format!("k{index}")).unwrap();
    }
    store.trim();
    let after = store.total_size();
    assert!(after < before, "trim did not shrink: {before} -> {after}");
    assert_eq!(store.count(), 20);
    assert_eq!(
        store.get_str("k499").as_deref(),
        Some("y".repeat(300).as_str())
    );

    store.sync();
    drop(store);
    let reopened = open(dir.path(), "trim", Mode::SINGLE_PROCESS);
    assert_eq!(reopened.count(), 20);
    assert_eq!(
        reopened.get_str("k490").as_deref(),
        Some("y".repeat(300).as_str())
    );
}

#[test]
fn compare_before_set_avoids_appends() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path(), "compare", Mode::SINGLE_PROCESS);
    store.enable_compare_before_set().unwrap();
    store.put_str("key", "value").unwrap();
    let size = store.actual_size();
    for _ in 0..50 {
        store.put_str("key", "value").unwrap();
    }
    assert_eq!(store.actual_size(), size);
    store.put_str("key", "value2").unwrap();
    assert!(store.actual_size() > size);
    assert_eq!(store.get_str("key").as_deref(), Some("value2"));
}

#[test]
fn special_character_ids_use_subdirectory() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path(), "user/a:b", Mode::SINGLE_PROCESS);
    store.put_str("key", "value").unwrap();
    store.sync();
    drop(store);

    assert!(dir.path().join("specialCharacter").is_dir());
    let entries = std::fs::read_dir(dir.path().join("specialCharacter"))
        .unwrap()
        .flatten()
        .count();
    assert_eq!(entries, 2, "expected data + crc files");

    let reopened = open(dir.path(), "user/a:b", Mode::SINGLE_PROCESS);
    assert_eq!(reopened.get_str("key").as_deref(), Some("value"));
}

#[test]
fn value_sizes_match_mmkv_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path(), "sizes", Mode::SINGLE_PROCESS);
    store.put_str("key", "hello").unwrap();
    store.put_i32("int", 300).unwrap();

    // String value blob is varint(5) + "hello" => 6 bytes; actual size 5.
    assert_eq!(store.value_size("key", false), 6);
    assert_eq!(store.value_size("key", true), 5);
    // int32 300 is a two-byte varint; it is not length-delimited.
    assert_eq!(store.value_size("int", false), 2);
    assert_eq!(store.value_size("int", true), 2);
    assert_eq!(store.value_size("missing", false), 0);

    let mut buffer = [0u8; 16];
    assert_eq!(store.write_value("key", &mut buffer).unwrap(), 5);
    assert_eq!(&buffer[..5], b"hello");
}
