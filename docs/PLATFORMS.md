# 平台落地指南

门面 crate 为 **`mmkv-rs`**（Rust 导入名 `mmkv_rs`）。所有平台共享同一套 API；
平台 crate 由 `cfg(target_os)` 自动选择。

## macOS / tvOS / watchOS / visionOS

```rust
mmkv_rs::init(mmkv_rs::InitOptions::default())?;
let store = mmkv_rs::default_store(&mmkv_rs::StoreConfig::new())?;
```

- 根目录：`NSDocumentDirectory/mmkv`（tvOS 为 `NSCachesDirectory/mmkv`）。
- 无需手动建目录，初始化时通过 `NSFileManager` 创建。
- 文件保护、后台通知在 macOS 上是 no-op；相关逻辑由 iOS crate 负责。

## iOS

```rust
mmkv_rs::init(mmkv_rs::InitOptions::default())?;
```

- 根目录：`NSDocumentDirectory/mmkv`。
- App Group：`InitOptions { group_dir: Some(Path::new(<container>)), .. }`，
  根为 `<container>/mmkv`。
- 可写数据文件与 `.crc` 自动设置
  `NSFileProtectionCompleteUntilFirstUserAuthentication`。
- 自动订阅 `UIApplicationDidEnterBackground` / `DidBecomeActive` /
  `DidReceiveMemoryWarning`；初始化时读取 `UIApplicationState`（App Extension 跳过）。
- App Extension 容器由 Foundation 的 DocumentDirectory 自动给出；跨宿主共享请用
  App Group 路径。

## Linux

```rust
mmkv_rs::init(mmkv_rs::InitOptions {
    root_dir: Some(std::path::Path::new("/var/lib/myapp/mmkv")),
    ..Default::default()
})?;
```

- MMKV 的 POSIX 构建没有隐式根目录，必须显式传入。
- 备份/恢复使用 `flock` + 文件复制；恢复路径使用 `sendfile`。
- 多进程通过 `.crc` 上的 `flock` 协调。

## Android

### 由 JNI Context 初始化（推荐）

```rust
let platform = mmkv_rs::android::AndroidPlatform::from_java(&vm, &context)?;
mmkv_rs::init_with(std::sync::Arc::new(platform), mmkv_rs::InitOptions::default())?;
```

- 根目录：`Context.getFilesDir()/mmkv`；临时目录：`Context.getCacheDir()`。
- 如果应用已经有路径，可直接：
  ```rust
  let platform = mmkv_rs::android::AndroidPlatform::new(files_dir, cache_dir);
  mmkv_rs::init_with(Arc::new(platform), mmkv_rs::InitOptions::default())?;
  ```
- 也支持 `AndroidPlatform::from_env()`（读取 `MMKV_ROOT` / `MMKV_CACHE`）。
- 进程模式检查：同一 `.crc` 上的 OFD record lock（offset 1/2）会拒绝
  `SINGLE_PROCESS` 与 `MULTI_PROCESS` 混用，返回 `Error::ProcessModeMismatch`。
- legacy 文件名（历史 `<md5>` 命名）在打开时自动迁移。

### ashmem store

```rust
let store = mmkv_rs::open(
    "shared-cache",
    &mmkv_rs::StoreConfig::new()
        .mode(mmkv_rs::Mode::ASHMEM)
        .expected_capacity(256 * 1024),
)?;
```

- 使用 `ASharedMemory_create`（失败时回退 `/dev/ashmem`），固定大小，不可 `trim`/扩容。
- 容量由 `expected_capacity` 决定；不足时写入返回错误。
- **跨进程 FD 交接**（ContentProvider / ParcelableMMKV）属于绑定层，本项目未实现；
  当前 ashmem store 在创建它的进程内使用。

## Windows

```rust
mmkv_rs::init(mmkv_rs::InitOptions {
    root_dir: Some(std::path::Path::new(r"C:\ProgramData\MyApp\mmkv")),
    ..Default::default()
})?;
```

- 无隐式根目录，必须显式传入。
- `CreateFileMappingW` / `MapViewOfFile`，`LockFileEx` 实现共享/独占锁。
- 路径使用 Rust `Path`（内部 UTF-16），ASCII `mmapID` 与 C++ 行为一致；
  含非 ASCII 的特殊字符 id 的 MD5 文件名与 C++ 宽字符哈希存在边缘差异。

## 多进程

- 所有平台：多进程实例通过 `.crc` 文件锁协调；读共享、写独占。
- 变更检测：`sequence` 变化 → 全量重载；CRC/actualSize 变化 → 增量加载；
  文件增长后自动重新 mmap。
- 单进程实例不会感知其他进程写入（与 MMKV 一致）；需要感知时使用
  `Mode::MULTI_PROCESS` 并调用 `check_content_changed()`。

## 参考测试

- 本机（macOS）：`cargo test --workspace`
- 跨平台编译：
  ```bash
  cargo check -p mmkv-rs --target aarch64-apple-ios
  cargo check -p mmkv-rs --target x86_64-unknown-linux-gnu
  cargo check -p mmkv-rs --target aarch64-linux-android
  cargo check -p mmkv-rs --target x86_64-pc-windows-msvc
  ```
