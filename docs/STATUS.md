# 实施状态（基线：C++ MMKV v2.4.3）

> 最后更新：2026-11-09
> 版本：`mmkv-rs` 0.1.0
> 验证：**71 个测试通过**，`clippy --workspace --all-targets -D warnings` 干净
> 说明：本项目面向 Rust 应用，不包含语言绑定。CI 见
> `.github/workflows/ci.yml`：Linux/macOS/Windows 跑 `fmt`/`clippy`/`test`，
> iOS/Android 做交叉编译检查。

## 平台矩阵

| 平台 | crate | 状态 | 验证方式 |
|---|---|---|---|
| macOS / tvOS / watchOS / visionOS | `mmkv-platform-macos` | 完成 | 本地运行测试 |
| iOS | `mmkv-platform-ios` | 完成 | `cargo check --target aarch64-apple-ios` |
| Linux | `mmkv-platform-linux` | 完成 | `cargo check --target x86_64-unknown-linux-gnu` |
| Android | `mmkv-platform-android` | 完成（ashmem 跨进程 FD 传递除外） | `cargo check --target aarch64-linux-android` |
| Windows | `mmkv-platform-windows` | 完成 | `cargo check --target x86_64-pc-windows-msvc` |
| 其他 POSIX | `mmkv-platform-posix` | 基础实现 | 编译检查 |

## 已完成

### 架构

- `mmkv-platform`：dyn-safe `Platform` / `MappedRegion` / `AppLifecycle`，锁类型
  (`Advisory` / `Record`)、进程模式检查、legacy 迁移、ashmem、平台复制。
- `mmkv-rs`：门面，按 `cfg(target_os)` 选择平台；`init` / `init_with` /
  `open` / `default_store` / `namespace` / 静态工具；`android`/`ios`/`linux`/
  `windows`/`apple` 平台 crate 重导出。
- 无 Swift shim、无 `cc`、无 Objective-C++ 构建步骤；Apple 全部使用 objc2。

### 格式与存储

- 追加日志、data-holder 双长度前缀、tombstone、重复 key 后者胜出。
- 112 字节 meta、version/sequence/actualSize/CRC/IV/flags。
- zlib CRC-32（链式，覆盖 payload/ciphertext）。
- 页对齐、扩容、全量写回、`trim`、`clearAll(keepSpace)`、`sync`/`async`、
  `clearMemoryCache`。
- 特殊字符 id、只读实例、`Namespace`、`is_file_valid`/`exists`/
  `remove_storage`（关闭缓存实例并持 `.crc` 独占锁）。
- backup/restore one/all：源 `.crc` 共享锁、目标 `.crc` 独占锁，恢复走
  `preserve_inode` 复制。
- `import_from`：按路径排序双实例加锁，避免 A↔B 死锁。
- `value_size`、`write_value`、`item_size_limit`、`compare_before_set`。
- 读取到过期 key 时立即追加 tombstone（与 C++ 一致）。
- 空闲实例自动清理：`enable_auto_cleanup` / `disable_auto_cleanup` /
  `try_auto_cleanup_instances`，后台状态跳过。
- 不可读文件隔离：打开可写实例前探测前 4 字节，异常文件重命名为
  `.corrupt.<timestamp>`（近似 C++ Apple `checkFileHasDiskError`）。

### 加密

- AES-CFB128，AES-128/256，原始 key 截断/零填充，无 KDF。
- 随机 IV、`meta.m_vector`、legacy key-IV 兼容。
- crypt holder：≤27 内联、≤256 缓存、>256 offset 惰性解密。
- 加密追加、删除、全量写回、`rekey`（明文↔密文、换 key、128↔256）、
  `reset_crypt_key`。
- **流式**：解码使用 16 字节窗口 + O(1) `status_at`，全量写回逐条写入/加密，
  不再分配整段明文/密文缓冲。
- NIST CFB 向量、C++ 加密 fixtures、在 C++ 加密文件上继续追加均通过。

### 过期

- 尾部 `u32 LE` 到期时间，0 = never。
- 读取即删、`all_keys_non_expired`/`count_non_expired`、`enumerate_keys_non_expired`。
- `enable_expiration`/`disable_expiration` 对非空 store 全量迁移（明文/加密）。
- 全量写回/扩容前过滤过期 key。
- 加密 + 过期组合测试。

### 多进程

- `.crc` 上的 `flock`（POSIX/Android 普通文件）或 `LockFileEx`（Windows）；
  Android ashmem 使用 `fcntl` record lock。
- 写操作先取独占进程锁，再刷新 meta，避免“刷新后被其他进程追加”的丢写竞态。
- sequence 变更全量重载；CRC/actualSize 变更增量加载；文件增长后重新 mmap。
- **真实双进程压力测试**：两个独立 OS 进程各写 250 条，最终 500 条无丢失。

### 平台细节

- macOS/iOS：`NSDocumentDirectory/mmkv`；App Group `<container>/mmkv`；
  `NSFileManager` 建目录；`NSTemporaryDirectory`。
- iOS：可写数据/`.crc` 设置
  `NSFileProtectionCompleteUntilFirstUserAuthentication`；订阅后台/前台/内存警告；
  初始化时查询 `UIApplicationState`；`.appex` 检测。
- Linux：显式根目录；`sendfile` 恢复复制。
- Android：`AndroidPlatform::new(root, cache)`、`from_java(vm, context)`、
  `from_env()`；`Context.getFilesDir()/mmkv`；进程模式检查（OFD record lock
  offset 1/2）；legacy `<md5>` 文件迁移；ashmem 区域
  (`ASharedMemory_create` + `/dev/ashmem` fallback)。
- Windows：`CreateFileMappingW`/`MapViewOfFile`、`LockFileEx`、页大小、
  宽字符路径由 `Path` 处理；显式根目录。

### 第二轮 review（let it crash）

- last-confirmed 恢复后同步修复 meta CRC，后续追加不会因 CRC 种子错误丢失。
- `reset_crypt_key` 清缓存后重载，外部 rekey 后内存字典不会停留在旧 key。
- handler 重入改为明确 panic，不再自死锁；回调文档化为纯通知。
- iOS 平台事件观察者保留到进程结束，内存警告/前后台跟踪生效。
- registry 缓存 key 与 `store.root` 使用同一个规范化路径。
- Windows `resize` 先 unmap 再 `SetEndOfFile`，失败恢复旧映射。
- `put_*` 刷新 `last_access`。
- 终审：`rekey` 与启用/禁用过期迁移失败时回滚内存状态，不会出现
  “内存认为已生效、文件没生效”的半状态。

## 测试

| 套件 | 数量 | 内容 |
|---|---|---|
| `mmkv-core` 单元 | 27 | varint/MiniPB、CRC、meta、NIST AES-CFB128、CFB 状态、流式解密、holder、handler 重入 |
| `fixtures.rs` | 9 | C++ v2.4.3 生成的明文/加密/过期文件 |
| `features.rs` | 21 | 加密往返、C++ 加密追加、rekey/reset、过期迁移/读取即删、备份恢复、多进程刷新、last-confirmed 恢复 |
| `multi_process.rs` | 1 | 两个独立进程并发写 |
| `roundtrip.rs` | 7 | 明文增长/trim/compare/value size |
| `apple_platform.rs` | 6 | objc2 路径、页大小、建目录、App Group |
| 合计 | **71** | |

跨 target 编译检查：

```bash
cargo check -p mmkv-rs --target aarch64-apple-ios
cargo check -p mmkv-rs --target x86_64-unknown-linux-gnu
cargo check -p mmkv-rs --target aarch64-linux-android
cargo check -p mmkv-rs --target x86_64-pc-windows-msvc
```

## 仍需注意

完整差异与理由见 [GAPS.md](GAPS.md)。当前版本的主要剩余项：

- 非 macOS 平台只做了编译验证，未在真机/模拟器运行。
- Android ashmem 跨进程 FD 传递（ContentProvider/JNI）未实现。
- Apple mmap 磁盘错误处理为“pread 探测 + 隔离”的近似实现。
- 无语言绑定（按用户要求）。CI 只覆盖编译，不含 iOS 模拟器/Android 模拟器运行测试。
- NSObject/NSCoding 等 ObjC 对象 API 不在 Rust API 范围内。
