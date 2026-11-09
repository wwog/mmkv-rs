# 缺失内容 / 差异清单

> 对照基线：C++ MMKV **v2.4.3**（`MMKV/Core` + iOS/Android/POSIX 平台层）
> 当前完成度：macOS 平台 + 核心存储/加密/过期/多进程/备份功能 1:1；其余平台与生态绑定未完成。
> 优先级：P0 = 阻塞“可用/正确”，P1 = 重要功能，P2 = 完善度/性能/工程化。

## 1. 平台与系统集成

| # | 缺失项 | 说明 | 优先级 |
|---|---|---|---|
| 1.1 | **iOS 平台 crate** | 当前 facade 只对 `target_os = "macos"` 选择 macOS 实现；iOS 会落到 `mmkv-platform-posix`，默认目录不可用。需要独立 `mmkv-platform-ios` 并在 facade 中按 `target_os` 选择。 | P0 |
| 1.2 | **iOS 文件保护** | `NSFileProtectionCompleteUntilFirstUserAuthentication` 只应在 iOS 可写文件上设置（数据 + `.crc`）；macOS 实现是 no-op。 | P0 |
| 1.3 | **iOS 后台/内存警告** | `AppLifecycle` 契约存在，但没有任何平台实现订阅 `UIApplicationDidEnterBackgroundNotification` / `DidBecomeActive` / `DidReceiveMemoryWarning`。当前只有 `mmkv_rs::init` 注册了一个空的内存警告回调。 | P1 |
| 1.4 | **自动空闲实例清理** | `enableAutoCleanUp(maxIdleMinutes)` / `disableAutoCleanUp` / `tryAutoCleanUpInstances` 未实现；iOS 后台状态对其的抑制逻辑也未实现。 | P2 |
| 1.5 | **Linux 平台 crate** | `mmkv-platform-posix` 可工作，但无独立 crate、无 `renameat2`/`sendfile` 优化、无 CI；默认根目录按 MMKV 语义必须显式传入。 | P1 |
| 1.6 | **Android 平台 crate** | 缺少 JNI/`Context` 获取 `filesDir/mmkv`、`cacheDir` 临时目录、`android_get_device_api_level`；目前只能显式传根目录。 | P0（Android 目标时） |
| 1.7 | **Android ashmem** | `mmkvWithAshmemFD`、固定大小 ashmem、fd 跨进程传递、ContentProvider/ParcelableMMKV 均未实现。 | P1（Android 目标时） |
| 1.8 | **Android 进程模式检查** | `m_fileModeLock` / `checkProcessMode` / `enableDisableProcessMode` 未实现；`MULTI_PROCESS` 与 `SINGLE_PROCESS` 混用不会被检测。 | P1（Android 目标时） |
| 1.9 | **Android legacy 文件迁移** | 历史上 Android 误用 `mmapKey` 作为文件名；`tryMigrateLegacyMMKVFile` 未实现。 | P2 |
| 1.10 | **Windows 平台 crate** | `mmkv-platform-posix` 在 Windows 无法编译（Unix `flock`/`OpenOptionsExt`）；需要 `LockFileEx`、`CreateFileMapping`、宽字符路径。 | P1（Windows 目标时） |
| 1.11 | **Apple App Extension / watchOS / tvOS 构建** | `.appex` 检测已有；没有独立 app-extension target、watchOS/tvOS CI 与构建配置。 | P2 |
| 1.12 | **默认目录之外的平台策略** | App Group 使用 `containerURLForSecurityApplicationGroupIdentifier`；MMKV 的 ObjC `groupDir` 是路径，Rust facade 已改为路径语义，但 iOS crate 仍需覆盖 `NSFileManager` 路径解析。 | P1 |

## 2. 存储 / 格式细节

| # | 缺失/差异 | 说明 | 优先级 |
|---|---|---|---|
| 2.1 | 非 UTF-8 key/value 支持 | 内部 key 是 `Box<[u8]>`，但公开 API 只接受 `&str`；`all_keys` 对非法 UTF-8 使用 lossy 转换。C++ `std::string` 可保存任意字节。 | P2 |
| 2.2 | `getString` 原地写入 | 没有 `get_str_into(&mut String)`；当前 `get_str` 分配新 `String`。语义正确，性能略差。 | P2 |
| 2.3 | 零拷贝读取 | 计划中的 `with_str` / `with_bytes` 闭包 API 未实现；`get_bytes` 总是复制。C++ `MMBuffer` 也有复制路径，但 `getBytes` 可选 no-copy。 | P2 |
| 2.4 | `enumerateKeys` / `enumerateNonExpiredKeys` | 未提供迭代器/回调形式；只有 `all_keys` 一次性收集。 | P2 |
| 2.5 | 读取时立即删除过期 key | C++ `getDataWithoutMTimeForKey` 在读到过期 key 时调用 `removeValueForKey` 追加 tombstone；当前只返回 `None`，等下次全量写回/`filter_expired` 才清理。 | P1 |
| 2.6 | `count(filterExpire=true)` 的写回副作用 | C++ 在可写实例上会触发只清理过期的全量写回；当前 `count_non_expired` 只做内存过滤，不压缩文件。外部可见结果一致，文件布局不同。 | P2 |
| 2.7 | 全量写回顺序与占位 varint | 当前按 key 排序重建、固定 4 字节 `0x200000` 占位；C++ 按 offset 顺序 memmove，并保留 legacy 文件最小 offset ≤5 的占位宽度。语义兼容（C++ 可读），非字节级一致。 | P2 |
| 2.8 | 旧式 meta/legacy 极端路径 | `version<3` 的 4 字节 actualSize 读写已支持；`version≥7` 重置、`lastConfirmed` 恢复已支持。未覆盖 `writeActualSize` 的所有边界分支（如异常 IV/sequence 组合）。 | P2 |
| 2.9 | `totalSize` 与 `actualSize` | 已按 C++ 语义：`total_size` = 文件大小，`actual_size` = 元数据 actualSize。 | — |
| 2.10 | 大文件/整型边界 | `maxActualSize` 用 `u32::MAX`；C++ 额外考虑 `size_t` 与页对齐。32 位目标未测试。 | P2 |
| 2.11 | 只读打开缺失文件 | C++ 返回一个空实例；当前 `Store::open_at` 在 `.crc` 不存在时返回 `FileNotValid`。 | P2 |

## 3. 加密

| # | 缺失/差异 | 说明 | 优先级 |
|---|---|---|---|
| 3.1 | 密钥内存清理 | `CryptKey` 在 Drop 时清零，但 `AesCrypt` 内部的 32 字节 key material 未 `zeroize`；C++ v2.4.3 有 secure key cleanup。 | P1 |
| 3.2 | 加密解码内存占用 | `decode_entries_crypt` 一次性解密整个 payload 到临时 `Vec<u8>`；C++ 用流式缓冲区，只保留小窗口。大 store 峰值内存约 +actualSize。 | P1（大文件） |
| 3.3 | 加密全量写回内存占用 | `write_all_entries_locked` 构造完整明文 + 完整密文缓冲；C++ memmove 按区段处理。大 store 峰值内存约 +2×actualSize。 | P1（大文件） |
| 3.4 | CFB 反向 AES / `statusBeforeDecrypt` | Rust 顺序解码 + `status_at`，无需反向 AES；行为等价，但未实现 C++ 的预读回滚路径（性能取舍）。 | P2 |
| 3.5 | `cryptKey()` 含 NUL 的 key | C++ 用 `strnlen` 截到首个 NUL；Rust 保留完整字节。极端边界差异。 | P2 |
| 3.6 | 硬件 AES | 依赖 RustCrypto `aes` 的后端；未显式验证 AES-NI/AArch64 加速与 ARMv8 asm 等价性（NIST 向量已过）。 | P2 |
| 3.7 | 加密 + backup/restore | 文件复制本身不区分加密，功能可用；未做专门的加密备份对照测试。 | P2 |
| 3.8 | IV 复用回归 | 已通过 `crypt_single` fixture（C++ 25 次更新）与 Rust 单 key 测试覆盖。 | — |

## 4. 过期

| # | 缺失/差异 | 说明 | 优先级 |
|---|---|---|---|
| 4.1 | 读取即删 | 见 2.5。 | P1 |
| 4.2 | 迁移失败/超限处理 | C++ 在 `enableAutoKeyExpire` 迁移时检查 `maximumMigrationSize`、`encodedEntrySize` 并拒绝过大数据；当前迁移只依赖 `write_all_entries_locked` 的 `TooLarge` 错误。 | P2 |
| 4.3 | `expiredInSeconds` 持久化 | 与 MMKV 一致：不落盘，重开需重新配置；当前已如此。 | — |
| 4.4 | 过期 + compare-before-set | 已按 MMKV 禁用。 | — |
| 4.5 | 过期 + 加密 | 已实现并测试。 | — |

## 5. 多进程 / 锁

| # | 缺失/差异 | 说明 | 优先级 |
|---|---|---|---|
| 5.1 | `removeStorage` 锁 | C++ 对 `.crc` 取独占 `flock` 后再 unlink；当前只关闭缓存实例，然后直接删文件，存在与其它进程写入竞争窗口。 | P1 |
| 5.2 | backup/restore 锁 | C++ 备份时对源 `.crc` 取共享锁、恢复时对目标取独占锁；当前 `std::fs::copy` 无锁。 | P1 |
| 5.3 | `importFrom` 锁顺序 | C++ 按存储路径排序后依次加锁，避免 A↔B 并发导入死锁；当前逐 key 读写，未做双实例原子加锁。 | P1 |
| 5.4 | `checkProcessMode` | 见 1.8。 | P1（Android） |
| 5.5 | `fcntl`/OFD byte-range 锁 | 仅实现 `flock`；Android ashmem/进程模式/迁移锁需要 `F_OFD_SETLK`/`F_SETLK`。 | P1（Android） |
| 5.6 | 真实多进程压力测试 | 当前测试在同一进程内开两个实例（不同 open file description，可验证 `flock` 冲突），但没有独立 OS 进程并发追加/回收压力。 | P1 |
| 5.7 | 内容变更回调 | `Handler::on_content_changed` 已接入；未实现 Java/ObjC 的全局通知分发。 | P2 |

## 6. API 表面

| # | 缺失项 | 说明 | 优先级 |
|---|---|---|---|
| 6.1 | Apple 对象 API | `NSObject<NSCoding>`、`NSDate`、`NSString`/`NSData` 类型化 get/set；当前 `NSDate` 只能通过 `put_f64(timeIntervalSince1970)` 表达。 | P1（Apple） |
| 6.2 | `migrateFromUserDefaultsDictionaryRepresentation` | ObjC 辅助 API 未实现。 | P2 |
| 6.3 | `importFromSharedPreferences` | Android 辅助 API 未实现（绑定层）。 | P2 |
| 6.4 | `enumerateKeys` | 见 2.4。 | P2 |
| 6.5 | `lock_thread` / `unlock_thread` | 内部 `parking_lot::Mutex`，未暴露 C++ 的线程锁 API。 | P2 |
| 6.6 | `MMKVMode` Android 位 | `CONTEXT_MODE_MULTI_PROCESS`、`MMKV_ASHMEM`、`MMKV_BACKUP` 未定义/未使用。 | P1（Android） |
| 6.7 | `MMKV::backupMultiProcessMMKV` 等命名 | 通用 backup/restore 已覆盖；没有单独的 multi-process 便捷函数。 | P2 |
| 6.8 | 返回风格 | `put_* -> Result<()>`、`get_*_opt -> Option<T>`、`remove -> Result<bool>`；与 C++ `bool` 语义有意不同（已按 Rust 约定）。 | — |

## 7. Apple 对象模型

| # | 缺失项 | 说明 | 优先级 |
|---|---|---|---|
| 7.1 | `NSCoding` 归档 | `NSKeyedArchiver` 对象存储未支持；需要 `mmkv-apple` 桥接层或 `objc2` 实现。 | P1（Apple） |
| 7.2 | `NSDate` 类型化 API | 核心只存 `f64`；缺少 `put_date` / `get_date`。 | P2 |
| 7.3 | `allKeysObjC` / `removeValuesForKeys(NSArray)` / block 枚举 | ObjC 专用表面未提供。 | P2 |
| 7.4 | Swift 名称/`NS_SWIFT_NAME` | Rust 无 Swift 导出；若做 Swift 绑定需在 FFI 层处理。 | P2 |

## 8. 恢复 / 容错

| # | 缺失项 | 说明 | 优先级 |
|---|---|---|---|
| 8.1 | Apple mmap 读错误检测 | `isDiskOfMMAPFileCorrupted`（`EDEVERR/EILSEQ/EINVAL/ENXIO`）未实现；C++ 会重命名/删除损坏文件并重载。 | P1 |
| 8.2 | `deleteOrRenameFile` | 损坏文件删除失败时的唯一重命名逻辑未实现。 | P2 |
| 8.3 | `checkFileHasDiskError` / 二次加载 | 未实现 C++ 的第二次机会重载路径。 | P2 |
| 8.4 | 文件长度/CRC 恢复策略 | `Discard`/`Recover` 与 `lastConfirmed` 已实现；未覆盖 C++ 的全部异常分支（如 `version` 异常组合）。 | P2 |
| 8.5 | 损坏文件的 C++ 对照测试 | fixtures 未包含截断/篡改/`version>=7` 场景。 | P2 |

## 9. 绑定 / 生态

| # | 缺失项 | 说明 | 优先级 |
|---|---|---|---|
| 9.1 | `mmkv-ffi` C ABI | 尚未创建；目标是对齐 `Core/cbridge/MMKVBridge.h` 的所有函数与 `mmkv_free` 所有权规则。 | P1 |
| 9.2 | Swift Package 包装 | 未创建。 | P2 |
| 9.3 | Android JNI / Kotlin 包装 | 未创建。 | P2 |
| 9.4 | Flutter / Dart、Python、KMP | 未创建。 | P2 |
| 9.5 | 跨语言互操作测试 | 当前只有 Rust 测试与 C++ fixtures；没有 Swift/Kotlin 端到端测试。 | P2 |

## 10. 测试 / 工程化

| # | 缺失项 | 说明 | 优先级 |
|---|---|---|---|
| 10.1 | Rust→C++ 持续差分测试 | 临时 C++ bridge 已按用户要求移除；当前只有 C++→Rust fixtures。Rust 写入的加密文件没有持续的 C++ 读回验证（移除前已完成一次双向验证）。 | P1 |
| 10.2 | Fuzz | 未对 payload 解码、meta、加密流做 `cargo-fuzz`。 | P2 |
| 10.3 | 独立 OS 进程并发压力 | 见 5.6。 | P1 |
| 10.4 | Benchmark | 未建立启动/追加/读取/全量写回/加密基准。 | P2 |
| 10.5 | CI | 仓库无 CI 配置；未跑 macOS arm64/x86_64、Linux、iOS 模拟器矩阵。 | P1 |
| 10.6 | Miri / sanitizer | mmap/AES unsafe 部分未跑 Miri/ASAN。 | P2 |
| 10.7 | 真实 iOS 设备测试 | 未做文件保护属性、App Group、扩展容器验证。 | P1（iOS） |

## 11. 性能取舍（正确但非最优）

| # | 项 | 说明 | 优先级 |
|---|---|---|---|
| 11.1 | 加密解码/写回整段缓冲 | 见 3.2/3.3。 | P1（大文件） |
| 11.2 | HashMap hasher | 使用 std 默认 SipHash；可用 `foldhash`/`FxHashMap` 提速。 | P2 |
| 11.3 | CRC-32 | 自实现逐字节表；可换 `crc32fast`（需确认与 zlib 链式语义一致）或 ARMv8 CRC 指令。 | P2 |
| 11.4 | mmap 读写路径 | 公开 getter 总是复制值；无可配置零拷贝。 | P2 |
| 11.5 | 全量写回 | 加密路径重建而非 C++ 的 memmove；普通路径已用 memmove。 | P2 |

## 12. 明确省略（设计取舍）

- **Android ashmem / ContentProvider**：依赖 JNI 与 fd 传递，属于绑定层，不是纯 Rust 核心。
- **Apple `NSCoding`**：需要 ObjC 运行时与归档格式，计划放到独立 `mmkv-apple` 桥接层。
- **32 位目标**：MMKV 2.x 要求 64 位；Rust 侧未验证 32 位 `usize` 边界。
- **Windows 宽字符路径**：等待 `mmkv-platform-windows`。
- **Flutter/Dart、Python、KMP**：不在首期范围。

## 13. 建议的下一批工作（按优先级）

1. **iOS 平台 crate**：objc2 路径/文件保护/生命周期；facade `cfg` 选择；iOS 模拟器测试。
2. **`mmkv-ffi` + Swift 包装**：先保证 Apple 生态可用。
3. **真实多进程压力测试 + `removeStorage`/backup/import 锁**。
4. **加密大文件流式化**（解密窗口 + 分段写回），降低峰值内存。
5. **读取即删过期 key、`enumerateKeys`、`with_bytes` 零拷贝**。
6. **Linux crate 与 CI 矩阵**，然后 Android（含 ashmem/进程检查）。
7. **Rust→C++ 持续验证**：可在 `tools/` 提供可选 C++ harness（不进入 workspace、不依赖 `cc`），CI 手动启用。
