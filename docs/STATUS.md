# 实施状态（基线：C++ MMKV v2.4.3）

> 最后更新：2026-11-09
> 当前状态：**核心功能 1:1 完成（macOS 平台），62 个测试全绿，clippy `-D warnings` 干净**

## 已完成

### 架构

- [x] `mmkv-platform`：dyn-safe 的 `Platform` / `MappedRegion` / `AppLifecycle` 契约。
- [x] `mmkv-platform-posix`：`MAP_SHARED` mmap、页对齐、显式零填充、`msync`、`flock`、`ftruncate`、`ftruncate` 不缩容打开。
- [x] `mmkv-platform-macos`：**objc2 + objc2-foundation**，无 Swift shim、无 `cc`、无 Objective-C++。
  - 默认根目录：`NSDocumentDirectory/mmkv`（tvOS 为 `NSCachesDirectory/mmkv`）。
  - App Group：`<container>/mmkv`。
  - `NSFileManager` 建目录，`NSTemporaryDirectory`，`.appex` 检测。
- [x] `mmkv-core`：格式、编解码、meta、CRC、明文/加密 store、注册表、namespace、备份。
- [x] `mmkv` 门面：`init` / `open` / `default_store` / `namespace` / 静态工具。
- [x] `group_dir` 按 MMKV 语义接收**目录路径**，根为 `<group_dir>/mmkv`。

### 格式与存储

- [x] 数据文件：4 字节旧式 actualSize + 伪 map-size varint + 追加记录流。
- [x] 条目编码、data-holder 双长度前缀、tombstone、重复 key 后者胜出。
- [x] 112 字节 meta、version/sequence/actualSize/CRC/IV/flags。
- [x] zlib CRC-32（链式、覆盖 payload/ciphertext）。
- [x] 页对齐、扩容翻倍、全量写回、`trim`、`clearAll(keepSpace)`、`sync`/`async`、`clearMemoryCache`。
- [x] 特殊字符 id、默认 id、只读模式、`Namespace`、`is_file_valid`、`exists`、`remove_storage`（会关闭缓存实例）。
- [x] backup / restore one/all（含 `specialCharacter` 子目录）。
- [x] `import_from`、`value_size`、`write_value`、`item_size_limit`。

### 加密（AES-CFB128）

- [x] AES-128/256，密钥按 MMKV 规则截断/零填充，无 KDF。
- [x] 整段 payload 连续 CFB 流，`num = offset % 16`；用 O(1) `status_at` 从密文重建状态，无需反向 AES。
- [x] 随机 IV（每次全量写回轮换），`meta.m_vector`；legacy version<2 使用 key 前 16 字节。
- [x] 加密 holder：≤27 内联、≤256 堆缓存、>256 offset 惰性解密。
- [x] 加密追加、加密全量写回、删除 tombstone、`rekey`（明文↔密文、换 key、AES-128↔256）、`reset_crypt_key`。
- [x] 加密下禁止 compare-before-set（与 MMKV 一致）。
- [x] 通过 NIST AES-CFB128 向量与 C++ 生成的 AES-128/256 fixtures。

### 过期

- [x] 尾部 `u32 LE` 到期时间；0 = never。
- [x] 读取时过滤、`all_keys_non_expired` / `count_non_expired`。
- [x] `enable_expiration` / `disable_expiration` 对非空 store 做全量迁移（明文与加密均支持）。
- [x] 全量写回/扩容前 `filterExpiredKeys`。
- [x] 加密 + 过期组合。

### 多进程

- [x] `.crc` fd 上的 `flock`，共享/独占锁、升级/降级、递归计数。
- [x] sequence 变更 → 全量重载；CRC/actualSize 变更 → 增量 `partialLoad`。
- [x] 文件增长后多进程重新 mmap（`actual_file_size` 对比）。
- [x] `lock()` / `try_lock()` RAII guard。
- [x] 测试覆盖追加、全量写回、锁冲突。

## 验证

- `cargo test --workspace`：**62 passed / 0 failed**。
- `cargo clippy --workspace --all-targets -- -D warnings`：干净。
- C++ fixtures 直接读取：plaintext、bulk、trim、clear、special id、crypt128、crypt256、crypt_bulk、crypt_single、expire。
- 加密 fixture 上继续追加、删除、重新打开均正确（验证续写 C++ 产生的 CFB 流）。
- 早期在 C++ bridge 仍存在时，已完成 Rust↔C++ 双向读写验证（所有原始类型、批量数据、特殊 id、多进程模式）；该验证结果保留为 fixtures 与文档，当前工作区不再依赖 C++ 工具链。

## 仍未完成

见 [GAPS.md](GAPS.md)。摘要：

1. iOS / Linux / Android / Windows 平台 crate。
2. iOS 文件保护、后台状态、内存警告、App Extension 生命周期。
3. Android ashmem、进程模式检查、legacy 文件迁移、Context/JNI 默认目录。
4. 自动空闲实例清理、Apple mmap 磁盘错误检测、原子备份/恢复锁。
5. NSObject/NSCoding/NSDate 对象 API、`enumerateKeys`。
6. C ABI / Swift / Kotlin 绑定。
7. Rust→C++ 的持续差分测试（当前只有 C++→Rust fixtures）、fuzz、benchmark。
8. 若干性能取舍（加密解码整段临时明文、全量写回重建而非 memmove）。
