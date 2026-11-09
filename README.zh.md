# mmkv-rs

[English](README.md) | 简体中文

[Tencent MMKV](https://github.com/Tencent/MMKV) **v2.4.3** 的 Rust 移植，磁盘格式
**逐字节兼容**。C++ MMKV v2.4.3 写出的数据可被本 crate 读取（包括 AES-CFB128 与
key-expiration 存储）；在参考桥仍然存在时，Rust 写出的明文存储也对照 C++ 实现做过验证。

架构沿用 GPUI 的平台拆分方式：单一 `Platform` 契约，每个受支持的操作系统对应一个 crate。
Foundation 访问使用 **`objc2`** —— 不需要 Swift shim，不需要 Objective-C++ 构建步骤，
也不依赖 `cc`。

本 crate 面向 **Rust 应用**。语言绑定（C ABI、Swift、Kotlin、Flutter、Python）与 CI
有意不在范围内。

## Crates

| Crate | 职责 |
|---|---|
| `mmkv-rs` | 对外门面：`init`、`open`、`default_store`、`Namespace`、静态辅助函数 |
| `mmkv-core` | 引擎：格式、MiniPB 编解码、CRC-32、meta、明文 + AES-CFB128 存储、注册表、备份、自动清理 |
| `mmkv-platform` | 平台契约（`Platform`、`MappedRegion`、`AppLifecycle`、锁类型） |
| `mmkv-platform-posix` | 共享的 POSIX mmap + `flock` + `ftruncate` 原语 |
| `mmkv-platform-macos` | macOS / tvOS / watchOS / visionOS，基于 `objc2-foundation` |
| `mmkv-platform-ios` | iOS：`NSDocumentDirectory/mmkv`、文件保护、后台/内存警告生命周期 |
| `mmkv-platform-linux` | Linux：POSIX 原语 + `sendfile` 恢复拷贝 |
| `mmkv-platform-android` | Android：JNI 路径、进程模式检查、旧版迁移、ashmem |
| `mmkv-platform-windows` | Windows：`CreateFileMappingW` / `MapViewOfFile` / `LockFileEx` |

## 平台根目录

| 平台 | 默认根目录 |
|---|---|
| macOS / iOS / watchOS / visionOS | `NSDocumentDirectory/mmkv` |
| tvOS | `NSCachesDirectory/mmkv` |
| iOS App Group | `<container>/mmkv` |
| Android | 通过 JNI 使用 `Context.getFilesDir()/mmkv`，或显式指定路径 |
| Linux / Windows | 显式根目录（MMKV 没有隐式的 POSIX/Windows 根目录） |

## 快速开始

macOS / iOS / tvOS：

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

Linux / Windows（显式根目录）：

```rust
mmkv_rs::init(mmkv_rs::InitOptions {
    root_dir: Some(std::path::Path::new("/var/lib/myapp/mmkv")),
    ..Default::default()
})?;
```

由应用自行提供 platform 的 Android / iOS：

```rust
// Android: resolve filesDir/cacheDir through JNI once at startup.
let platform = mmkv_rs::android::AndroidPlatform::from_java(&vm, &context)?;
mmkv_rs::init_with(std::sync::Arc::new(platform), mmkv_rs::InitOptions::default())?;
```

加密存储：

```rust
let key = mmkv_rs::CryptKey::new(b"0123456789abcdef".to_vec());
let store = mmkv_rs::open(
    "com.example.secrets",
    &mmkv_rs::StoreConfig::new().crypt_key(&key, false), // false = AES-128
)?;
store.put_str("token", "secret")?;
```

## 已实现

- 追加写日志、data-holder 编码、tombstone、全量回写、扩容、`trim`、
  `clearAll(keepSpace)`、`clearMemoryCache`。
- 112 字节 `.crc` 元数据、zlib CRC-32、sequence/actualSize/IV/flags。
- AES-CFB128，支持 AES-128/256 密钥，每次重写使用随机 IV，crypt holder
  （≤27 内联、≤256 缓存、>256 惰性偏移）、`rekey`、`reset_crypt_key`。
- 流式加密解码（16 字节窗口）与逐条加密重写，大存储无需分配整文件明文缓冲。
- Key 过期（逐 key 时间戳、读取时删除、启用/禁用迁移、仅枚举未过期项）、
  compare-before-set、单条 item 大小限制。
- 多进程：在 `.crc` 上使用 `flock`/`LockFileEx`，内容变更检测，增量与全量重载，
  扩容后重新映射，独占锁/尝试锁。
- 备份/恢复/删除锁，`importFrom` 的确定性双锁。
- 空闲实例自动清理（`enable_auto_cleanup`）。
- 特殊字符 id、只读实例、`Namespace`、`is_file_valid`、`exists`、
  `remove_storage`、备份/恢复、导入。
- Android ashmem 区域、进程模式检查、旧版 `<md5>` 文件迁移。
- iOS `NSFileProtectionCompleteUntilFirstUserAuthentication`、后台与内存警告通知、
  App Extension 检测。

## 测试

```bash
cargo test --workspace
```

65 个测试：

| 测试套件 | 覆盖内容 |
|---|---|
| `crates/mmkv-rs/tests/fixtures.rs` | 读取上游 C++ MMKV v2.4.3 产出的存储：明文、扩容/trim/clear、特殊 id、AES-128/256、大 value、单 key IV 轮换、过期 |
| `crates/mmkv-rs/tests/features.rs` | 加密往返、向 C++ 加密 fixture 追加写入、rekey、过期迁移/读时删除、带锁的备份/恢复、多进程刷新、错误密钥安全性 |
| `crates/mmkv-rs/tests/multi_process.rs` | 两个独立的操作系统进程向同一存储追加写入（各 250 次） |
| `crates/mmkv-rs/tests/roundtrip.rs` | 明文追加日志、扩容、trim、compare-before-set、value 大小 |
| `crates/mmkv-core` 单元测试 | varint/MiniPB、CRC-32、meta 布局、NIST AES-CFB128、CFB 状态重建、流式解密、holder 阈值 |
| `crates/mmkv-platform-macos/tests/apple_platform.rs` | objc2 路径、页大小、目录创建、App Group 处理 |

五个平台 crate 均通过以下命令交叉检查：

```bash
cargo check -p mmkv-rs --target aarch64-apple-ios
cargo check -p mmkv-rs --target x86_64-unknown-linux-gnu
cargo check -p mmkv-rs --target aarch64-linux-android
cargo check -p mmkv-rs --target x86_64-pc-windows-msvc
```

`crates/mmkv-rs/tests/fixtures/` 下的二进制文件由上游 C++ core 在临时测试桥移除之前生成，
参见 `fixtures/README.md`。

## 文档

- [平台指南](docs/PLATFORMS.md) —— 各操作系统设置、JNI/ashmem、根目录。
- [迁移计划](docs/PORTING_PLAN.md) —— 格式/架构分析与分阶段路线图。
- [实施状态](docs/STATUS.md) —— 各平台已完成并验证的内容。
- [差异清单](docs/GAPS.md) —— 剩余差异、排除项与需要运行时验证的部分。

## 兼容性说明

- 磁盘字节、文件名、`.crc` 元数据、CRC-32 语义与路径编码均与 MMKV v2.4.3 一致。
- MMKV 采用 BSD-3-Clause 许可；本移植保留原许可与署名。
