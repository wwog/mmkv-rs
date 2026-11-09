# 缺失内容 / 差异清单（v0.1.0 之后）

> 基线：C++ MMKV **v2.4.3**
> 本轮已完成 P0/P1 及大部分 P2 核心项；语言绑定与 CI 按用户要求不做。
> 下文只列仍然存在的差异。已完成项见 [STATUS.md](STATUS.md)。

## 1. 运行时验证（P1，工程性）

| # | 项 | 说明 |
|---|---|---|
| 1.1 | iOS 真机/模拟器 | 仅通过 `cargo check --target aarch64-apple-ios`；文件保护属性、App Group、后台/内存警告、`UIApplicationState` 初始查询均未在运行时验证。 |
| 1.2 | Android 真机/模拟器 | 仅通过 `cargo check --target aarch64-linux-android`；`from_java`、`from_env`、进程模式检查、legacy 迁移、ashmem 均未运行。 |
| 1.3 | Linux | 仅通过 `cargo check --target x86_64-unknown-linux-gnu`；`sendfile` 恢复路径、`flock`、多进程未运行。 |
| 1.4 | Windows | 仅通过 `cargo check --target x86_64-pc-windows-msvc`；`CreateFileMappingW`、`LockFileEx`、删除打开中的 `.crc`（share-delete）未运行。 |
| 1.5 | 32 位目标 | MMKV 2.x 要求 64 位；Rust 侧未验证 32 位 `usize`/`off_t` 边界。 |

## 2. Android ashmem 跨进程（P1）

| # | 项 | 说明 |
|---|---|---|
| 2.1 | FD 传递 | `AshmemRegion`、`Platform::open_ashmem`、`Mode::ASHMEM` 已实现，但缺少 `mmkvWithAshmemFD` / ContentProvider / ParcelableMMKV 的跨进程 fd 交接；当前 ashmem store 只在创建它的进程内可用。 |
| 2.2 | ashmem 管理 API | 没有 `ashmemFD()`/`ashmemMetaFD()` 导出；`is_file_valid`/`exists`/`remove_storage`/backup/restore 对 ashmem 无意义（ashmem 无文件路径）。 |
| 2.3 | ashmem 进程模式检查 | `check_process_mode` 只对文件路径生效；ashmem 的进程模式检查未实现。 |
| 2.4 | ashmem 固定大小 | 容量由 `expected_capacity` 决定；不足时全量写回返回错误（与 C++ ashmem 一致）。 |

## 3. Apple 对象模型（范围外）

| # | 项 | 说明 |
|---|---|---|
| 3.1 | `NSObject<NSCoding>` | 未提供。Rust 核心只处理原始类型/字符串/字节/数值数组；`NSDate` 可用 `f64`（Unix 秒）表达。 |
| 3.2 | `NSDate` 类型化 API | 无 `put_date`/`get_date`；可用 `put_f64(date.timeIntervalSince1970)`。 |
| 3.3 | `allKeysObjC` / block 枚举 / `removeValuesForKeys(NSArray)` | ObjC 专用表面未实现；Rust 侧提供 `enumerate_keys` / `enumerate_keys_non_expired`。 |
| 3.4 | `migrateFromUserDefaultsDictionaryRepresentation` | ObjC 辅助 API 未实现。 |

> 这些是 Objective-C 对象 API，不属于“给 Rust 项目使用”的范围；若将来需要，可新增 `mmkv-apple` 桥接层。

## 4. 语言绑定 / 生态（按用户要求不做）

- `mmkv-ffi` C ABI（对齐 `Core/cbridge/MMKVBridge.h`）。
- Swift Package、Kotlin/JNI、Flutter/Dart、Python、KMP。
- 跨语言端到端测试。
- CI（用户明确不需要）。

## 5. 恢复 / 容错精度（P2）

| # | 项 | 说明 |
|---|---|---|
| 5.1 | Apple mmap 磁盘错误 | 当前是“打开前 `pread` 4 字节探测 + 异常重命名”近似方案；未实现 C++ `isDiskOfMMAPFileCorrupted` 的 `EDEVERR/EILSEQ/EINVAL/ENXIO` 判定，也无法捕获 mmap 访问时的 SIGBUS。 |
| 5.2 | `deleteOrRenameFile` | 删除失败后的唯一重命名逻辑未完整复刻；当前直接重命名到 `.corrupt.<timestamp>`。 |
| 5.3 | `checkFileHasDiskError` 二次加载 | 未实现 C++ 的第二次机会重载路径。 |
| 5.4 | 备份/恢复原子性 | 默认 `copy_file` 不是“临时文件 + 原子 rename”；Linux 备份走 `std::fs::copy`（恢复走 `sendfile`）。APFS clone / `renameat2(RENAME_EXCHANGE)` 优化未使用。 |
| 5.5 | `removeStorage` Windows | 依赖 Rust `File` 的 share-delete 语义；未在 Windows 运行时验证删除成功。 |
| 5.6 | 只读打开缺失文件 | C++ 返回空实例；当前 `open_at` 在 `.crc` 缺失时返回 `FileNotValid`。 |

## 6. 格式/行为细节（P2）

| # | 项 | 说明 |
|---|---|---|
| 6.1 | `all_keys` / `count` 的 filterExpire 写回副作用 | Rust 的 `all_keys_non_expired`/`count_non_expired` 只做内存过滤；C++ 在可写实例上可能触发“只清理过期”的全量写回。外部计数一致，文件布局不同。 |
| 6.2 | 全量写回顺序 | 按 key 排序重建；C++ 按旧 offset 顺序 memmove。C++ 可读，非字节级一致。 |
| 6.3 | 占位 varint | 固定 `0x200000`（4 字节）；C++ legacy 文件最小 offset ≤5 时可能沿用更窄/更宽占位。语义兼容。 |
| 6.4 | `cryptKey()` 含 NUL | C++ 用 `strnlen` 截到首个 NUL；Rust 保留完整 key 字节。 |
| 6.5 | 非 UTF-8 key/value | 内部 key 为 `Box<[u8]>`，但公开 API 接受 `&str`；`all_keys` 对非法 UTF-8 使用 lossy 转换。C++ `std::string` 可保存任意字节。 |
| 6.6 | Windows 非 ASCII mmapID | 特殊字符 id 的 MD5 在 C++ Windows 上对 UTF-16 字节哈希；Rust 对 UTF-8 哈希。纯 ASCII id 不受影响。 |
| 6.7 | `MMKV_BACKUP` | `Mode::BACKUP` 已定义以接受该位，但与 C++ 一样是 no-op。 |
| 6.8 | 只读/空键错误返回 | `put` 空 key 返回 `Err(InvalidId)`；C++ 返回 `false`。Rust API 风格差异。 |

## 7. 性能 / 内存（P2）

| # | 项 | 说明 |
|---|---|---|
| 7.1 | 加密解码 | 已改为 16 字节窗口流式读取；每字节一次分支，未做块级批量拷贝优化。 |
| 7.2 | 加密全量写回 | 已改为逐条写入/加密；每条 entry 分配一次临时 `Vec`，峰值 ≈ 最大单条 value，而非整库。 |
| 7.3 | 零拷贝读取 | `with_bytes`/`with_str` 仍先把值复制到 `Vec` 再回调；未暴露 mmap 切片生命周期 API。 |
| 7.4 | HashMap hasher | std 默认 SipHash；可用 `foldhash`/`FxHashMap` 提速。 |
| 7.5 | CRC-32 | 逐字节表实现；未使用 ARMv8 CRC 指令或 `crc32fast`。 |
| 7.6 | 基准/fuzz | 无 benchmark、无 `cargo-fuzz`、无 Miri/sanitizer。 |

## 8. 其他 API 差异（P2）

| # | 项 | 说明 |
|---|---|---|
| 8.1 | `lock_thread` / `unlock_thread` | 内部 `parking_lot::Mutex`，未暴露 C++ 的线程锁 API。 |
| 8.2 | `MMKV::onAppTerminate` | 使用 `shutdown()`。 |
| 8.3 | `importFromSharedPreferences` | Android 辅助 API 未实现（绑定层）。 |
| 8.4 | `backupMultiProcessMMKV` 等命名 | 通用 backup/restore 已覆盖。 |
| 8.5 | `enableCompareBeforeSet` 返回值 | Rust 返回 `Result<()>`；C++ 返回 `bool`。 |
| 8.6 | `remove`/`put` 返回值 | Rust 使用 `Result`/`Option`；C++ 使用 `bool`（有意按 Rust 风格）。 |
| 8.7 | `PageSize`/`version` | `mmkv_rs::page_size()` / `VERSION`。 |
| 8.8 | `onExit` | `mmkv_rs::shutdown()`。 |

## 9. 测试覆盖缺口（P2）

- 没有 Rust→C++ 的持续差分测试（C++ bridge 已移除）；当前只有 C++→Rust fixtures。
- 没有损坏文件 fixtures（截断、CRC 篡改、meta version≥7、随机字节）。
- 没有加密 + 备份/恢复的专门对照。
- 没有 Windows/Linux/Android/iOS 运行期测试。
- 多进程压力测试只覆盖普通追加；未覆盖跨进程 trim/clear/backup/restore 竞争。

## 10. 明确不做（按用户范围）

- 所有语言包装（C ABI、Swift、Kotlin、Flutter、Python、KMP）。
- CI 配置。
- Apple `NSCoding`/NSObject 对象 API。
- 32 位目标。
- Android ContentProvider / Parcelable / FD 跨进程交接（属于绑定层，见 §2）。
