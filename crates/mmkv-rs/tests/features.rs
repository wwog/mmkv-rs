//! Feature tests that do not require the C++ reference: encrypted stores,
//! rekey, expiration migration, backup/restore and multi-process refresh.

use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use mmkv_core::meta::{MetaInfo, VERSION_RANDOM_IV};
use mmkv_core::{CryptKey, Mode, Store, StoreConfig};
use mmkv_platform_macos::MacPlatform;

fn open(root: &Path, id: &str, mode: Mode, key: Option<(&CryptKey, bool)>) -> Arc<Store> {
    let platform = Arc::new(MacPlatform::new());
    let mut config = StoreConfig::new().mode(mode);
    if let Some((key, aes256)) = key {
        config = config.crypt_key(key, aes256);
    }
    Store::open_at(platform, root, id, &config).unwrap_or_else(|error| panic!("open {id}: {error}"))
}

fn copy_dir(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).unwrap();
    for entry in std::fs::read_dir(source).unwrap().flatten() {
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn fixtures_copy() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    copy_dir(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures"),
        dir.path(),
    );
    dir
}

#[test]
fn crypt_roundtrip_and_wrong_key() {
    let dir = tempfile::tempdir().unwrap();
    let key = CryptKey::new(b"0123456789abcdef".to_vec());
    let store = open(
        dir.path(),
        "crypt_roundtrip",
        Mode::SINGLE_PROCESS,
        Some((&key, false)),
    );

    store.put_bool("bool", true).unwrap();
    store.put_i32("i32", -42).unwrap();
    store.put_str("small", "hello").unwrap();
    store.put_str("large", &"X".repeat(5_000)).unwrap();
    store.put_bytes("bytes", &[1, 2, 3]).unwrap();
    store.put_str("removed", "gone").unwrap();
    store.remove("removed").unwrap();
    assert_eq!(store.count(), 5);
    store.sync();
    drop(store);

    let reopened = open(
        dir.path(),
        "crypt_roundtrip",
        Mode::SINGLE_PROCESS,
        Some((&key, false)),
    );
    assert!(reopened.is_encryption_enabled());
    assert!(reopened.get_bool("bool", false));
    assert_eq!(reopened.get_i32("i32", 0), -42);
    assert_eq!(reopened.get_str("small").as_deref(), Some("hello"));
    assert_eq!(reopened.get_str("large"), Some("X".repeat(5_000)));
    assert_eq!(reopened.get_bytes("bytes"), Some(vec![1, 2, 3]));
    assert_eq!(reopened.get_str("removed"), None);
    assert_eq!(
        reopened.crypt_key().as_deref(),
        Some(b"0123456789abcdef".as_slice())
    );
    assert!(Store::is_file_valid("crypt_roundtrip", dir.path()));

    let meta = std::fs::read(dir.path().join("crypt_roundtrip.crc")).unwrap();
    let meta = MetaInfo::read(&meta).unwrap();
    assert!(meta.version >= VERSION_RANDOM_IV);
    assert_ne!(meta.vector, [0u8; 16]);
}

#[test]
fn append_to_cpp_encrypted_fixture() {
    let dir = fixtures_copy();
    let key = CryptKey::new(b"0123456789abcdef".to_vec());
    let store = open(
        dir.path(),
        "crypt128",
        Mode::SINGLE_PROCESS,
        Some((&key, false)),
    );
    assert_eq!(store.get_str("small").as_deref(), Some("crypt-small"));
    assert_eq!(store.get_str("large").map(|value| value.len()), Some(5_000));

    store.put_str("appended", "from-rust").unwrap();
    store.put_str("appended-large", &"R".repeat(2_000)).unwrap();
    store.remove("small").unwrap();
    store.sync();
    drop(store);

    let reopened = open(
        dir.path(),
        "crypt128",
        Mode::SINGLE_PROCESS,
        Some((&key, false)),
    );
    assert_eq!(reopened.get_str("appended").as_deref(), Some("from-rust"));
    assert_eq!(reopened.get_str("appended-large"), Some("R".repeat(2_000)));
    assert_eq!(reopened.get_str("small"), None);
    assert_eq!(reopened.get_str("large"), Some("C".repeat(5_000)));
    assert_eq!(reopened.count(), 6);
}

#[test]
fn crypt_full_writeback_and_growth() {
    let dir = tempfile::tempdir().unwrap();
    let key = CryptKey::new(b"0123456789abcdef".to_vec());
    let store = open(
        dir.path(),
        "crypt_bulk_rust",
        Mode::SINGLE_PROCESS,
        Some((&key, false)),
    );

    let value = "Z".repeat(300);
    for index in 0..300 {
        store.put_str(&format!("k{index:03}"), &value).unwrap();
    }
    for index in 0..150 {
        store.remove(&format!("k{index:03}")).unwrap();
    }
    assert_eq!(store.count(), 150);
    store.sync();
    drop(store);

    let reopened = open(
        dir.path(),
        "crypt_bulk_rust",
        Mode::SINGLE_PROCESS,
        Some((&key, false)),
    );
    assert_eq!(reopened.count(), 150);
    assert_eq!(reopened.get_str("k299"), Some(value.clone()));
    assert_eq!(reopened.get_str("k000"), None);
    let meta = std::fs::read(dir.path().join("crypt_bulk_rust.crc")).unwrap();
    let meta = MetaInfo::read(&meta).unwrap();
    assert!(meta.sequence > 0);
    assert_ne!(meta.vector, [0u8; 16]);
}

#[test]
fn rekey_transitions() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path(), "rekey", Mode::SINGLE_PROCESS, None);
    store.put_str("key", "value").unwrap();
    store.put_str("large", &"L".repeat(1_000)).unwrap();
    store.put_i32("int", 7).unwrap();

    let key1 = CryptKey::new(b"0123456789abcdef".to_vec());
    store.rekey(Some(&key1), false).unwrap();
    assert!(store.is_encryption_enabled());
    assert_eq!(store.get_str("key").as_deref(), Some("value"));
    assert_eq!(store.get_str("large"), Some("L".repeat(1_000)));
    let size_after_rekey = store.actual_size();
    store.rekey(Some(&key1), false).unwrap();
    assert_eq!(store.actual_size(), size_after_rekey, "same key is a no-op");

    let key2 = CryptKey::new(b"0123456789abcdef0123456789abcdef".to_vec());
    store.rekey(Some(&key2), true).unwrap();
    assert_eq!(store.crypt_key().as_deref(), Some(key2.as_bytes()));
    assert_eq!(store.get_i32("int", 0), 7);
    store.sync();
    drop(store);

    let reopened = open(
        dir.path(),
        "rekey",
        Mode::SINGLE_PROCESS,
        Some((&key2, true)),
    );
    assert_eq!(reopened.get_str("key").as_deref(), Some("value"));
    assert_eq!(reopened.get_str("large"), Some("L".repeat(1_000)));

    reopened.rekey(None, false).unwrap();
    assert!(!reopened.is_encryption_enabled());
    assert_eq!(reopened.get_str("key").as_deref(), Some("value"));
    reopened.sync();
    drop(reopened);

    let plain = open(dir.path(), "rekey", Mode::SINGLE_PROCESS, None);
    assert!(!plain.is_encryption_enabled());
    assert_eq!(plain.get_str("key").as_deref(), Some("value"));
    assert_eq!(plain.get_i32("int", 0), 7);
}

#[test]
fn expiration_enable_and_disable_migrates_values() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path(), "expire_migrate", Mode::SINGLE_PROCESS, None);
    store.put_str("a", "one").unwrap();
    store.put_str("b", "two").unwrap();
    store.put_i32("c", 3).unwrap();

    store.enable_expiration(1).unwrap();
    assert!(store.is_expiration_enabled());
    assert_eq!(store.get_str("a").as_deref(), Some("one"));
    thread::sleep(Duration::from_secs(2));
    assert_eq!(store.get_str("a"), None);
    assert_eq!(store.count(), 3);
    assert_eq!(store.count_non_expired(), 0);

    store.disable_expiration().unwrap();
    assert!(!store.is_expiration_enabled());
    assert_eq!(store.get_str("a").as_deref(), Some("one"));
    assert_eq!(store.get_i32("c", 0), 3);
    store.sync();
    drop(store);

    let reopened = open(dir.path(), "expire_migrate", Mode::SINGLE_PROCESS, None);
    assert_eq!(reopened.get_str("b").as_deref(), Some("two"));
    assert_eq!(reopened.count_non_expired(), 3);
}

#[test]
fn expiration_with_encryption() {
    let dir = tempfile::tempdir().unwrap();
    let key = CryptKey::new(b"0123456789abcdef".to_vec());
    let store = open(
        dir.path(),
        "expire_crypt",
        Mode::SINGLE_PROCESS,
        Some((&key, false)),
    );
    store.enable_expiration(0).unwrap();
    store.put_str_expiring("short", "one-second", 1).unwrap();
    store.put_str_expiring("long", "one-hour", 3_600).unwrap();
    store.put_i32_expiring("num", 9, 3_600).unwrap();

    assert_eq!(store.get_str("short").as_deref(), Some("one-second"));
    thread::sleep(Duration::from_secs(2));
    assert_eq!(store.get_str("short"), None);
    assert_eq!(store.get_str("long").as_deref(), Some("one-hour"));
    assert_eq!(store.count_non_expired(), 2);
    store.sync();
    drop(store);

    let reopened = open(
        dir.path(),
        "expire_crypt",
        Mode::SINGLE_PROCESS,
        Some((&key, false)),
    );
    assert_eq!(reopened.get_str("short"), None);
    assert_eq!(reopened.get_i32("num", 0), 9);
}

#[test]
fn backup_and_restore() {
    let source = tempfile::tempdir().unwrap();
    let backup = tempfile::tempdir().unwrap();
    let restored = tempfile::tempdir().unwrap();

    let plain = open(source.path(), "backup_plain", Mode::SINGLE_PROCESS, None);
    plain.put_str("key", "plain").unwrap();
    plain.sync();
    drop(plain);

    let key = CryptKey::new(b"0123456789abcdef".to_vec());
    let crypt = open(
        source.path(),
        "backup_crypt",
        Mode::SINGLE_PROCESS,
        Some((&key, false)),
    );
    crypt.put_str("key", "crypt").unwrap();
    crypt.sync();
    drop(crypt);

    let special = open(source.path(), "backup/a:b", Mode::SINGLE_PROCESS, None);
    special.put_str("key", "special").unwrap();
    special.sync();
    drop(special);

    assert!(Store::backup_one(
        "backup_plain",
        backup.path(),
        source.path()
    ));
    assert!(Store::backup_one(
        "backup_crypt",
        backup.path(),
        source.path()
    ));
    assert!(Store::backup_one(
        "backup/a:b",
        backup.path(),
        source.path()
    ));
    assert!(!Store::backup_one("missing", backup.path(), source.path()));

    assert!(Store::restore_one(
        "backup_plain",
        backup.path(),
        restored.path()
    ));
    assert!(Store::restore_one(
        "backup_crypt",
        backup.path(),
        restored.path()
    ));
    assert!(Store::restore_one(
        "backup/a:b",
        backup.path(),
        restored.path()
    ));

    let plain = open(restored.path(), "backup_plain", Mode::SINGLE_PROCESS, None);
    assert_eq!(plain.get_str("key").as_deref(), Some("plain"));
    let crypt = open(
        restored.path(),
        "backup_crypt",
        Mode::SINGLE_PROCESS,
        Some((&key, false)),
    );
    assert_eq!(crypt.get_str("key").as_deref(), Some("crypt"));
    let special = open(restored.path(), "backup/a:b", Mode::SINGLE_PROCESS, None);
    assert_eq!(special.get_str("key").as_deref(), Some("special"));

    assert_eq!(Store::backup_all(backup.path(), source.path()), 3);
    let all = tempfile::tempdir().unwrap();
    assert_eq!(Store::restore_all(backup.path(), all.path()), 3);
}

#[test]
fn multi_process_sees_appends_and_full_writebacks() {
    let dir = tempfile::tempdir().unwrap();
    let writer = open(dir.path(), "multi", Mode::MULTI_PROCESS, None);
    let reader = open(dir.path(), "multi", Mode::MULTI_PROCESS, None);

    writer.put_str("first", "one").unwrap();
    writer.sync();
    reader.check_content_changed();
    assert_eq!(reader.get_str("first").as_deref(), Some("one"));

    reader.put_str("second", "two").unwrap();
    reader.sync();
    writer.check_content_changed();
    assert_eq!(writer.get_str("second").as_deref(), Some("two"));

    // Force a full write-back (sequence change) in one process.
    for index in 0..300 {
        writer
            .put_str(&format!("bulk-{index:03}"), &"M".repeat(300))
            .unwrap();
    }
    writer.sync();
    reader.check_content_changed();
    assert_eq!(reader.count(), 302);
    assert_eq!(reader.get_str("bulk-299"), Some("M".repeat(300)));

    let guard = writer.lock().unwrap();
    assert!(reader.try_lock().is_none());
    drop(guard);
    assert!(reader.try_lock().is_some());
}

#[test]
fn clear_memory_cache_reloads_plain_and_crypt() {
    let dir = tempfile::tempdir().unwrap();
    let key = CryptKey::new(b"0123456789abcdef".to_vec());

    for (id, encrypted) in [("cache_plain", false), ("cache_crypt", true)] {
        let store = if encrypted {
            open(dir.path(), id, Mode::SINGLE_PROCESS, Some((&key, false)))
        } else {
            open(dir.path(), id, Mode::SINGLE_PROCESS, None)
        };
        store.put_str("key", "value").unwrap();
        store.put_str("large", &"Q".repeat(1_000)).unwrap();
        store.clear_memory_cache(false);
        assert_eq!(store.get_str("key").as_deref(), Some("value"));
        assert_eq!(store.get_str("large"), Some("Q".repeat(1_000)));
    }
}

#[test]
fn import_between_plain_and_encrypted() {
    let dir = tempfile::tempdir().unwrap();
    let key = CryptKey::new(b"0123456789abcdef".to_vec());

    let plain = open(dir.path(), "import_plain", Mode::SINGLE_PROCESS, None);
    plain.put_str("s", "string").unwrap();
    plain.put_i32("i", -5).unwrap();
    plain.put_bytes("b", &[7, 8, 9]).unwrap();
    plain.sync();

    let crypt = open(
        dir.path(),
        "import_crypt",
        Mode::SINGLE_PROCESS,
        Some((&key, false)),
    );
    assert_eq!(crypt.import_from(&plain).unwrap(), 3);
    assert_eq!(crypt.get_str("s").as_deref(), Some("string"));
    assert_eq!(crypt.get_i32("i", 0), -5);
    assert_eq!(crypt.get_bytes("b"), Some(vec![7, 8, 9]));

    let back = open(dir.path(), "import_back", Mode::SINGLE_PROCESS, None);
    assert_eq!(back.import_from(&crypt).unwrap(), 3);
    assert_eq!(back.get_str("s").as_deref(), Some("string"));
    assert_eq!(back.get_i32("i", 0), -5);
}

#[test]
fn crypt_trim_and_clear() {
    let dir = tempfile::tempdir().unwrap();
    let key = CryptKey::new(b"0123456789abcdef".to_vec());
    let store = open(
        dir.path(),
        "crypt_trim",
        Mode::SINGLE_PROCESS,
        Some((&key, false)),
    );
    for index in 0..300 {
        store
            .put_str(&format!("k{index:03}"), &"T".repeat(300))
            .unwrap();
    }
    for index in 0..280 {
        store.remove(&format!("k{index:03}")).unwrap();
    }
    let before = store.total_size();
    store.trim();
    assert!(store.total_size() <= before);
    assert_eq!(store.count(), 20);
    assert_eq!(store.get_str("k299"), Some("T".repeat(300)));
    store.sync();
    drop(store);

    let reopened = open(
        dir.path(),
        "crypt_trim",
        Mode::SINGLE_PROCESS,
        Some((&key, false)),
    );
    assert_eq!(reopened.count(), 20);
    reopened.clear_keep_space();
    assert_eq!(reopened.count(), 0);
    reopened.put_str("after", "clear").unwrap();
    assert_eq!(reopened.get_str("after").as_deref(), Some("clear"));
}

#[test]
fn disable_expiration_with_encryption() {
    let dir = tempfile::tempdir().unwrap();
    let key = CryptKey::new(b"0123456789abcdef".to_vec());
    let store = open(
        dir.path(),
        "expire_crypt_disable",
        Mode::SINGLE_PROCESS,
        Some((&key, false)),
    );
    store.enable_expiration(1).unwrap();
    store.put_str("key", "value").unwrap();
    store.disable_expiration().unwrap();
    assert!(!store.is_expiration_enabled());
    assert_eq!(store.get_str("key").as_deref(), Some("value"));
    store.sync();
    drop(store);

    let reopened = open(
        dir.path(),
        "expire_crypt_disable",
        Mode::SINGLE_PROCESS,
        Some((&key, false)),
    );
    assert_eq!(reopened.get_str("key").as_deref(), Some("value"));
}

#[test]
fn compare_before_set_is_rejected_for_encrypted_stores() {
    let dir = tempfile::tempdir().unwrap();
    let key = CryptKey::new(b"0123456789abcdef".to_vec());
    let store = open(
        dir.path(),
        "compare_crypt",
        Mode::SINGLE_PROCESS,
        Some((&key, false)),
    );
    assert!(store.enable_compare_before_set().is_err());
    assert!(!store.is_compare_before_set_enabled());
}

#[test]
fn reset_crypt_key_after_external_rekey() {
    let dir = tempfile::tempdir().unwrap();
    let key1 = CryptKey::new(b"0123456789abcdef".to_vec());
    let key2 = CryptKey::new(b"fedcba9876543210".to_vec());

    let writer = open(
        dir.path(),
        "external_rekey",
        Mode::SINGLE_PROCESS,
        Some((&key1, false)),
    );
    writer.put_str("key", "value").unwrap();
    writer.sync();

    let reader = open(
        dir.path(),
        "external_rekey",
        Mode::SINGLE_PROCESS,
        Some((&key1, false)),
    );
    assert_eq!(reader.get_str("key").as_deref(), Some("value"));

    writer.rekey(Some(&key2), false).unwrap();
    writer.sync();
    reader.reset_crypt_key(Some(&key2), false).unwrap();
    reader.clear_memory_cache(false);
    assert_eq!(reader.get_str("key").as_deref(), Some("value"));
}

#[test]
fn wrong_crypt_key_read_only_does_not_destroy_data() {
    let dir = fixtures_copy();
    let wrong = CryptKey::new(b"wrong-key-000000".to_vec());
    let store = open(
        dir.path(),
        "crypt128",
        Mode::READ_ONLY,
        Some((&wrong, false)),
    );
    assert_eq!(store.count(), 0);
    assert_eq!(store.get_str("small"), None);

    let key = CryptKey::new(b"0123456789abcdef".to_vec());
    let store = open(dir.path(), "crypt128", Mode::READ_ONLY, Some((&key, false)));
    assert_eq!(store.get_str("small").as_deref(), Some("crypt-small"));
}
