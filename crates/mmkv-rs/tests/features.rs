//! Feature tests that do not require the C++ reference: encrypted stores,
//! rekey, expiration migration, backup/restore and multi-process refresh.

use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use mmkv_core::meta::{MetaInfo, VERSION_RANDOM_IV};
use mmkv_core::{CryptKey, Mode, Store, StoreConfig};

fn open(root: &Path, id: &str, mode: Mode, key: Option<(&CryptKey, bool)>) -> Arc<Store> {
    let platform = mmkv_rs::default_platform();
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
fn borrowed_reads_match_owned_reads() {
    let dir = tempfile::tempdir().unwrap();
    let key = CryptKey::new(b"0123456789abcdef".to_vec());
    for (id, encrypted) in [("borrow_plain", false), ("borrow_crypt", true)] {
        let store = if encrypted {
            open(dir.path(), id, Mode::SINGLE_PROCESS, Some((&key, false)))
        } else {
            open(dir.path(), id, Mode::SINGLE_PROCESS, None)
        };

        let small = "borrowed-small";
        let large = "B".repeat(700);
        store.put_str("small", small).unwrap();
        store.put_str("large", &large).unwrap();
        store.put_bytes("bin", &[0, 1, 2, 255]).unwrap();

        let mut seen = None;
        store.with_str("large", |value| seen = value.map(str::to_owned));
        assert_eq!(seen.as_deref(), Some(large.as_str()));

        let mut seen = None;
        store.with_str("small", |value| seen = value.map(str::to_owned));
        assert_eq!(seen.as_deref(), Some(small));

        store.with_bytes("bin", |value| {
            assert_eq!(value, Some(&[0u8, 1, 2, 255][..]));
        });
        store.with_bytes("missing", |value| assert!(value.is_none()));
        store.with_str("missing", |value| assert!(value.is_none()));

        assert_eq!(store.get_str("large"), Some(large.clone()));
        assert_eq!(store.get_bytes("bin"), Some(vec![0, 1, 2, 255]));
    }
}

#[test]
fn crypt_compaction_mixes_small_and_large_values() {
    let dir = tempfile::tempdir().unwrap();
    let key = CryptKey::new(b"0123456789abcdef".to_vec());
    let store = open(
        dir.path(),
        "crypt_compact_mixed",
        Mode::SINGLE_PROCESS,
        Some((&key, false)),
    );

    let large = "L".repeat(700);
    let grown = "M".repeat(900);
    for index in 0..120 {
        store.put_str(&format!("large{index:03}"), &large).unwrap();
        store.put_str(&format!("small{index:03}"), "tiny").unwrap();
    }
    // Overwrites and deletes leave dead records behind, so the next write-back
    // has to compact non-adjacent runs and rewrite both holder kinds.
    for index in 0..40 {
        store.put_str(&format!("large{index:03}"), &grown).unwrap();
        store.remove(&format!("small{index:03}")).unwrap();
    }
    store.trim();
    assert_eq!(store.count(), 200);

    for index in 0..40 {
        assert_eq!(
            store.get_str(&format!("large{index:03}")).as_deref(),
            Some(grown.as_str())
        );
        assert_eq!(store.get_str(&format!("small{index:03}")), None);
    }
    for index in 40..120 {
        assert_eq!(
            store.get_str(&format!("large{index:03}")).as_deref(),
            Some(large.as_str())
        );
        assert_eq!(
            store.get_str(&format!("small{index:03}")).as_deref(),
            Some("tiny")
        );
    }
    store.sync();
    drop(store);

    let reopened = open(
        dir.path(),
        "crypt_compact_mixed",
        Mode::SINGLE_PROCESS,
        Some((&key, false)),
    );
    assert_eq!(reopened.count(), 200);
    assert_eq!(
        reopened.get_str("large039").as_deref(),
        Some(grown.as_str())
    );
    assert_eq!(
        reopened.get_str("large040").as_deref(),
        Some(large.as_str())
    );
    assert_eq!(reopened.get_str("small040").as_deref(), Some("tiny"));
    assert_eq!(reopened.get_str("small000"), None);
}

#[test]
fn plain_compaction_reuses_offsets_with_gaps() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path(), "plain_compaction", Mode::SINGLE_PROCESS, None);

    for round in 0..4 {
        for index in 0..120 {
            let value = format!("r{round}-{}", "x".repeat((index * 13) % 700));
            store.put_str(&format!("key{index:03}"), &value).unwrap();
        }
        store.trim();
        for index in 0..120 {
            let expected = format!("r{round}-{}", "x".repeat((index * 13) % 700));
            assert_eq!(
                store.get_str(&format!("key{index:03}")),
                Some(expected),
                "round {round} key {index}"
            );
        }
    }

    for index in (0..120).step_by(3) {
        assert!(store.remove(&format!("key{index:03}")).unwrap());
    }
    store.trim();
    assert_eq!(store.count(), 80);
    for index in 0..120 {
        if index % 3 == 0 {
            assert_eq!(store.get_str(&format!("key{index:03}")), None);
        } else {
            let expected = format!("r3-{}", "x".repeat((index * 13) % 700));
            assert_eq!(
                store.get_str(&format!("key{index:03}")),
                Some(expected),
                "key {index}"
            );
        }
    }
    store.sync();
    drop(store);

    let reopened = open(dir.path(), "plain_compaction", Mode::SINGLE_PROCESS, None);
    assert_eq!(reopened.count(), 80);
    assert_eq!(
        reopened.get_str("key119"),
        Some(format!("r3-{}", "x".repeat((119 * 13) % 700)))
    );
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
fn read_only_open_with_expiration_config() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path(), "expire_readonly", Mode::SINGLE_PROCESS, None);
    store.enable_expiration(3_600).unwrap();
    store.put_str("key", "value").unwrap();
    store.sync();
    drop(store);

    let platform = mmkv_rs::default_platform();
    let read_only = Store::open_at(
        platform,
        dir.path(),
        "expire_readonly",
        &StoreConfig::new()
            .mode(Mode::READ_ONLY)
            .expiration(mmkv_rs::Expiration::After(3_600)),
    )
    .expect("read-only open with expiration config");
    assert_eq!(read_only.get_str("key").as_deref(), Some("value"));
    assert!(read_only.is_expiration_enabled());
}

#[test]
fn expiring_vector_values() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path(), "expire_vectors", Mode::SINGLE_PROCESS, None);
    store.enable_expiration(0).unwrap();
    store.put_i32_vec_expiring("ints", &[1, 2, 3], 1).unwrap();
    store
        .put_strings_expiring("strings", &["a".to_owned(), "b".to_owned()], 3_600)
        .unwrap();
    assert_eq!(store.get_i32_vec("ints"), Some(vec![1, 2, 3]));
    thread::sleep(Duration::from_secs(2));
    assert_eq!(store.get_i32_vec("ints"), None);
    assert_eq!(
        store.get_strings("strings"),
        Some(vec!["a".to_owned(), "b".to_owned()])
    );
}

#[test]
fn close_releases_process_locks() {
    let dir = tempfile::tempdir().unwrap();
    let first = open(dir.path(), "close_locks", Mode::MULTI_PROCESS, None);
    let second = open(dir.path(), "close_locks", Mode::MULTI_PROCESS, None);
    let guard = first.lock().unwrap();
    assert!(second.try_lock().is_none());
    drop(guard);
    assert!(second.try_lock().is_some());
    first.close();
    assert!(second.try_lock().is_some());
}

#[test]
fn expired_key_is_deleted_on_read() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path(), "expire_read", Mode::SINGLE_PROCESS, None);
    store.enable_expiration(0).unwrap();
    store.put_str_expiring("short", "one-second", 1).unwrap();
    store.put_str_expiring("long", "one-hour", 3_600).unwrap();
    assert_eq!(store.count(), 2);

    thread::sleep(Duration::from_secs(2));
    assert_eq!(store.get_str("short"), None);
    // Reading an expired key appends a tombstone (MMKV does the same).
    assert_eq!(store.count(), 1);
    store.sync();
    drop(store);

    let reopened = open(dir.path(), "expire_read", Mode::SINGLE_PROCESS, None);
    assert_eq!(reopened.count(), 1);
    assert_eq!(reopened.get_str("short"), None);
    assert_eq!(reopened.get_str("long").as_deref(), Some("one-hour"));
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
fn batch_remove_compacts_plain_and_crypt() {
    let dir = tempfile::tempdir().unwrap();
    let key = CryptKey::new(b"0123456789abcdef".to_vec());
    let value = "v".repeat(64);
    for (id, encrypted) in [("batch_plain", false), ("batch_crypt", true)] {
        let store = if encrypted {
            open(dir.path(), id, Mode::SINGLE_PROCESS, Some((&key, false)))
        } else {
            open(dir.path(), id, Mode::SINGLE_PROCESS, None)
        };
        for index in 0..60 {
            store.put_str(&format!("k{index:02}"), &value).unwrap();
        }
        let removed: Vec<String> = (0..30).map(|index| format!("k{index:02}")).collect();
        let removed: Vec<&str> = removed.iter().map(String::as_str).collect();
        store.remove_many(&removed).unwrap();
        assert_eq!(store.count(), 30, "{id}");
        assert_eq!(store.get_str("k00"), None);
        assert_eq!(store.get_str("k30").as_deref(), Some(value.as_str()));
        store.sync();
        drop(store);

        let reopened = if encrypted {
            open(dir.path(), id, Mode::SINGLE_PROCESS, Some((&key, false)))
        } else {
            open(dir.path(), id, Mode::SINGLE_PROCESS, None)
        };
        assert_eq!(reopened.count(), 30, "{id}");
        assert_eq!(reopened.get_str("k00"), None);
        assert_eq!(reopened.get_str("k59").as_deref(), Some(value.as_str()));
    }
}

#[test]
fn contains_and_large_values_respect_expiration() {
    let dir = tempfile::tempdir().unwrap();
    let key = CryptKey::new(b"0123456789abcdef".to_vec());
    for (id, encrypted) in [("contains_plain", false), ("contains_crypt", true)] {
        let store = if encrypted {
            open(dir.path(), id, Mode::SINGLE_PROCESS, Some((&key, false)))
        } else {
            open(dir.path(), id, Mode::SINGLE_PROCESS, None)
        };
        store.enable_expiration(0).unwrap();

        // A large expiring value uses an offset holder on the encrypted store;
        // reading it must go through the cheap trailer check only once.
        let large = "L".repeat(700);
        store.put_str_expiring("large", &large, 3_600).unwrap();
        store.put_str_expiring("short", "one-second", 1).unwrap();
        store.put_str("keep", "alive").unwrap();

        assert!(store.contains("short"));
        assert_eq!(store.get_str("large").as_deref(), Some(large.as_str()));
        assert_eq!(store.get_str("keep").as_deref(), Some("alive"));

        thread::sleep(Duration::from_secs(2));
        // C++ `containsKey` reports false and deletes an expired key.
        assert!(!store.contains("short"));
        assert_eq!(store.count(), 2, "{id}");
        assert!(store.contains("large"));

        store.sync();
        drop(store);

        let reopened = if encrypted {
            open(dir.path(), id, Mode::SINGLE_PROCESS, Some((&key, false)))
        } else {
            open(dir.path(), id, Mode::SINGLE_PROCESS, None)
        };
        assert_eq!(reopened.count(), 2, "{id}");
        assert!(!reopened.contains("short"));
        assert_eq!(reopened.get_str("large").as_deref(), Some(large.as_str()));
    }
}

#[test]
fn writeback_drops_expired_keys() {
    let dir = tempfile::tempdir().unwrap();
    let key = CryptKey::new(b"0123456789abcdef".to_vec());
    let store = open(
        dir.path(),
        "expire_writeback",
        Mode::SINGLE_PROCESS,
        Some((&key, false)),
    );
    store.enable_expiration(0).unwrap();

    let value = "V".repeat(400);
    for index in 0..30 {
        store
            .put_str_expiring(&format!("exp{index:02}"), &value, 1)
            .unwrap();
    }
    store.put_str("keep", "alive").unwrap();
    thread::sleep(Duration::from_secs(2));

    // Growing the file runs the expiration filter before compacting.
    for index in 0..80 {
        store.put_str(&format!("fill{index:02}"), &value).unwrap();
    }
    store.trim();
    assert_eq!(store.count(), 81);
    assert_eq!(store.count_non_expired(), 81);
    store.sync();
    drop(store);

    let reopened = open(
        dir.path(),
        "expire_writeback",
        Mode::SINGLE_PROCESS,
        Some((&key, false)),
    );
    assert_eq!(reopened.count(), 81);
    assert_eq!(reopened.get_str("keep").as_deref(), Some("alive"));
    assert_eq!(reopened.get_str("exp00"), None);
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

    let platform = mmkv_rs::default_platform();
    assert!(Store::backup_one(
        "backup_plain",
        backup.path(),
        source.path(),
        platform.as_ref()
    ));
    assert!(Store::backup_one(
        "backup_crypt",
        backup.path(),
        source.path(),
        platform.as_ref()
    ));
    assert!(Store::backup_one(
        "backup/a:b",
        backup.path(),
        source.path(),
        platform.as_ref()
    ));
    assert!(!Store::backup_one(
        "missing",
        backup.path(),
        source.path(),
        platform.as_ref()
    ));

    assert!(Store::restore_one(
        "backup_plain",
        backup.path(),
        restored.path(),
        platform.as_ref()
    ));
    assert!(Store::restore_one(
        "backup_crypt",
        backup.path(),
        restored.path(),
        platform.as_ref()
    ));
    assert!(Store::restore_one(
        "backup/a:b",
        backup.path(),
        restored.path(),
        platform.as_ref()
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

    assert_eq!(
        Store::backup_all(backup.path(), source.path(), platform.as_ref()),
        3
    );
    let all = tempfile::tempdir().unwrap();
    assert_eq!(
        Store::restore_all(backup.path(), all.path(), platform.as_ref()),
        3
    );
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
    // No explicit cache flush: reset_crypt_key must reload by itself.
    assert_eq!(reader.get_str("key").as_deref(), Some("value"));
}

#[test]
fn reset_crypt_key_handles_plain_and_cipher_directions() {
    let dir = tempfile::tempdir().unwrap();
    let key = CryptKey::new(b"0123456789abcdef".to_vec());

    let reader = open(
        dir.path(),
        "external_rekey_dirs",
        Mode::SINGLE_PROCESS,
        None,
    );
    reader.put_str("key", "value").unwrap();
    reader.put_str("large", &"L".repeat(1_000)).unwrap();
    reader.sync();

    // Another process turns the plain store into an encrypted one.
    let writer = open(
        dir.path(),
        "external_rekey_dirs",
        Mode::SINGLE_PROCESS,
        None,
    );
    writer.rekey(Some(&key), false).unwrap();
    writer.sync();
    drop(writer);

    reader.reset_crypt_key(Some(&key), false).unwrap();
    assert!(reader.is_encryption_enabled());
    assert_eq!(reader.get_str("key").as_deref(), Some("value"));
    assert_eq!(reader.get_str("large"), Some("L".repeat(1_000)));

    // And back to plaintext.
    let writer = open(
        dir.path(),
        "external_rekey_dirs",
        Mode::SINGLE_PROCESS,
        Some((&key, false)),
    );
    writer.rekey(None, false).unwrap();
    writer.sync();
    drop(writer);

    reader.reset_crypt_key(None, false).unwrap();
    assert!(!reader.is_encryption_enabled());
    assert_eq!(reader.get_str("key").as_deref(), Some("value"));

    // Writes after the reload must produce a valid store.
    reader.put_str("after", "reset").unwrap();
    reader.sync();
    drop(reader);
    let reopened = open(
        dir.path(),
        "external_rekey_dirs",
        Mode::SINGLE_PROCESS,
        None,
    );
    assert_eq!(reopened.get_str("after").as_deref(), Some("reset"));
    assert_eq!(reopened.get_str("large"), Some("L".repeat(1_000)));
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

#[test]
fn last_confirmed_recovery_keeps_following_appends() {
    let dir = tempfile::tempdir().unwrap();
    let store = open(dir.path(), "recover_last", Mode::SINGLE_PROCESS, None);
    for index in 0..200 {
        store
            .put_str(&format!("k{index:03}"), &"V".repeat(300))
            .unwrap();
    }
    // A full write-back records lastActualSize/lastCRC. `trim` forces one.
    store.trim();
    store.sync();
    drop(store);

    let crc_path = dir.path().join("recover_last.crc");
    let mut bytes = std::fs::read(&crc_path).unwrap();
    let mut meta = MetaInfo::read(&bytes).unwrap();
    assert!(meta.last_actual_size > 0);
    assert_eq!(
        meta.last_actual_size, meta.actual_size,
        "trim must leave a confirmed snapshot"
    );
    // Simulate the torn write that `checkDataValid` recovers from: the meta
    // CRC no longer matches, but the last confirmed CRC still does.
    meta.crc_digest ^= 0xFFFF_FFFF;
    meta.write(&mut bytes);
    std::fs::write(&crc_path, &bytes).unwrap();

    let store = open(dir.path(), "recover_last", Mode::SINGLE_PROCESS, None);
    assert_eq!(store.count(), 200);
    // The loader must repair the meta CRC to the recovered payload; this is
    // what lets the following append chain from a valid digest. Flush first so
    // the on-disk assertion does not depend on mapped-write visibility.
    store.sync();
    let repaired = MetaInfo::read(&std::fs::read(&crc_path).unwrap()).unwrap();
    assert_eq!(repaired.crc_digest, repaired.last_crc_digest);
    store.put_str("after", "survives").unwrap();
    store.sync();
    drop(store);

    let reopened = open(dir.path(), "recover_last", Mode::SINGLE_PROCESS, None);
    assert_eq!(reopened.get_str("after").as_deref(), Some("survives"));
    assert_eq!(reopened.count(), 201);
}
