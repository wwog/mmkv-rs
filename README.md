# mmkv-rs

English | [简体中文](README.zh.md)

A Rust port of [Tencent MMKV](https://github.com/Tencent/MMKV) **v2.4.3** with a
**byte-compatible on-disk format**. Stores written by C++ MMKV v2.4.3 are read
by this crate (including AES-CFB128 and key-expiration stores), and Rust-written
plaintext stores were verified against the C++ implementation while the
reference bridge was still present.

The architecture follows GPUI's platform split: a single `Platform` contract,
with one crate per supported OS. Foundation access uses **`objc2`** — no Swift
shim, no Objective-C++ build step, no `cc` dependency.

This crate targets **Rust applications**. Language bindings (C ABI, Swift,
Kotlin, Flutter, Python) are intentionally out of scope. CI runs the full test
suite on Linux, macOS and Windows, and compile-checks the iOS/Android targets.

## Crates

| Crate | Role |
|---|---|
| `mmkv-rs` | Public façade: `init`, `open`, `default_store`, `Namespace`, static helpers |
| `mmkv-core` | Engine: format, MiniPB codec, CRC-32, meta, plaintext + AES-CFB128 store, registry, backup, auto-cleanup |
| `mmkv-platform` | Platform contract (`Platform`, `MappedRegion`, `AppLifecycle`, lock kinds) |
| `mmkv-platform-posix` | Shared POSIX mmap + `flock` + `ftruncate` primitives |
| `mmkv-platform-macos` | macOS / tvOS / watchOS / visionOS via `objc2-foundation` |
| `mmkv-platform-ios` | iOS: `NSDocumentDirectory/mmkv`, file protection, background/memory-warning lifecycle |
| `mmkv-platform-linux` | Linux: POSIX primitives + `sendfile` restore copies |
| `mmkv-platform-android` | Android: JNI paths, process-mode check, legacy migration, ashmem |
| `mmkv-platform-windows` | Windows: `CreateFileMappingW` / `MapViewOfFile` / `LockFileEx` |

## Platform roots

| Platform | Default root |
|---|---|
| macOS / iOS / watchOS / visionOS | `NSDocumentDirectory/mmkv` |
| tvOS | `NSCachesDirectory/mmkv` |
| iOS App Group | `<container>/mmkv` |
| Android | `Context.getFilesDir()/mmkv` via JNI, or explicit path |
| Linux / Windows | explicit root (MMKV has no implicit POSIX/Windows root) |

## Quick start

macOS / iOS / tvOS:

```rust
fn main() -> mmkv_rs::Result<()> {
    // Default root is NSDocumentDirectory/mmkv, like MMKV itself.
    mmkv_rs::init(mmkv_rs::InitOptions::default())?;

    let store = mmkv_rs::default_store(&mmkv_rs::StoreConfig::new())?;
    store.put_str("user", "alice")?;
    store.put_i64("counter", 42)?;
    store.sync();

    assert_eq!(store.get_str("user").as_deref(), Some("alice"));
    assert_eq!(store.get_i64("counter", 0), 42);
    Ok(())
}
```

Linux / Windows (explicit root):

```rust
mmkv_rs::init(mmkv_rs::InitOptions {
    root_dir: Some(std::path::Path::new("/var/lib/myapp/mmkv")),
    ..Default::default()
})?;
```

Android / iOS with app-provided platform:

```rust
// Android: resolve filesDir/cacheDir through JNI once at startup.
let platform = mmkv_rs::android::AndroidPlatform::from_java(&vm, &context)?;
mmkv_rs::init_with(std::sync::Arc::new(platform), mmkv_rs::InitOptions::default())?;
```

Encrypted store:

```rust
let key = mmkv_rs::CryptKey::new(b"0123456789abcdef".to_vec());
let store = mmkv_rs::open(
    "com.example.secrets",
    &mmkv_rs::StoreConfig::new().crypt_key(&key, false), // false = AES-128
)?;
store.put_str("token", "secret")?;
```

## Implemented

- Append-only log, data-holder encoding, tombstones, full write-back, growth,
  `trim`, `clearAll(keepSpace)`, `clearMemoryCache`.
- 112-byte `.crc` metadata, zlib CRC-32, sequence/actualSize/IV/flags.
- AES-CFB128 with AES-128/256 keys, random IV per rewrite, crypt holders
  (≤27 inline, ≤256 cached, >256 lazy offset), `rekey`, `reset_crypt_key`.
- Streamed encrypted decode (16-byte window) and per-entry encrypted rewrite,
  so large stores do not allocate whole-file plaintext buffers.
- Key expiration (per-key timestamps, read-time deletion, enable/disable
  migration, non-expired enumeration), compare-before-set, item size limit.
- Multi-process `flock`/`LockFileEx` on `.crc`, content-change detection,
  incremental and full reloads, remapping after growth, exclusive/try locks.
- Backup/restore/removal locks, `importFrom` deterministic dual locking.
- Idle-instance auto cleanup (`enable_auto_cleanup`).
- Special-character ids, read-only instances, `Namespace`, `is_file_valid`,
  `exists`, `remove_storage`, backup/restore, import.
- Android ashmem regions, process-mode check, legacy `<md5>` file migration.
- iOS `NSFileProtectionCompleteUntilFirstUserAuthentication`, background and
  memory-warning notifications, App Extension detection.

## Tests

```bash
cargo test --workspace
```

68 tests:

| Suite | Coverage |
|---|---|
| `crates/mmkv-rs/tests/fixtures.rs` | Reads stores produced by upstream C++ MMKV v2.4.3: plaintext, growth/trim/clear, special ids, AES-128/256, large values, single-key IV rotation, expiration |
| `crates/mmkv-rs/tests/features.rs` | Crypt round-trips, appending to a C++ encrypted fixture, rekey, expiration migration/read-deletion, backup/restore with locks, multi-process refresh, wrong-key safety |
| `crates/mmkv-rs/tests/multi_process.rs` | Two separate OS processes append to the same store (250 writes each) |
| `crates/mmkv-rs/tests/roundtrip.rs` | Plaintext append log, growth, trim, compare-before-set, value sizes |
| `crates/mmkv-core` unit tests | varint/MiniPB, CRC-32, meta layout, NIST AES-CFB128, CFB status reconstruction, streaming decrypt, holder thresholds |
| `crates/mmkv-platform-macos/tests/apple_platform.rs` | objc2 paths, page size, directory creation, App Group handling |

All five platform crates are cross-checked with:

```bash
cargo check -p mmkv-rs --target aarch64-apple-ios
cargo check -p mmkv-rs --target x86_64-unknown-linux-gnu
cargo check -p mmkv-rs --target aarch64-linux-android
cargo check -p mmkv-rs --target x86_64-pc-windows-msvc
```

The binaries in `crates/mmkv-rs/tests/fixtures/` were generated by the upstream
C++ core before the temporary test bridge was removed; see
`fixtures/README.md`.

## Documentation

- [Platform guide](docs/PLATFORMS.md) — per-OS setup, JNI/ashmem, roots.
- [Migration plan](docs/PORTING_PLAN.md) — format/architecture analysis and phased roadmap.
- [Implementation status](docs/STATUS.md) — what is done and verified per platform.
- [Code review](docs/REVIEW.md) — method/logic consistency and Rust-adaptation findings.
- [Gap list](docs/GAPS.md) — remaining differences, exclusions and runtime-validation needs.

## Compatibility notes

- On-disk bytes, file names, `.crc` metadata, CRC-32 semantics and path encoding
  match MMKV v2.4.3.
- MMKV is BSD-3-Clause; this port keeps the license and attribution.
