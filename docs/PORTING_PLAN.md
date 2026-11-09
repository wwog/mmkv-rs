# MMKV → Rust 移植落地方案

> 基线：`MMKV/`（Tencent MMKV **v2.4.3**，Core `MMKV_VERSION`）
> 目标：**功能 1:1**、**磁盘格式字节级兼容**、**Rust 风格 API**、**平台分层（GPUI 式）**
> 本文档只描述方案，不包含实现代码；阶段划分与验收标准见 §6。
>
> **实施决策更新（2026-11-09，最终）**：门面 crate 命名为 **`mmkv-rs`**；平台目录获取使用
> **objc2 / objc2-foundation**，不使用 Swift shim；iOS/Android/Linux/Windows 均已有独立平台 crate；
> C++ 参考桥接已移除，兼容性由 C++ 生成的 **golden fixtures** 持续验证。
> 按项目定位（Rust 应用），**不提供 C ABI/Swift/Kotlin/Flutter/Python 包装，也不添加 CI**。
> 下文保留原始方案作为设计记录，其中“Swift shim / C++ bridge / FFI”段落仅作历史参考。

---

## 0. 结论摘要

1. MMKV 的本质是：**mmap 文件 + 追加式 protobuf 记录流 + 全量写回收缩 + 独立 `.crc` meta 文件**。没有数据库引擎，没有 WAL，没有 B-Tree。
2. 兼容性的关键不在容器，而在 **9 个字节级不变量**：数据文件前 4 字节、条目编码、holder 双长度前缀、meta 结构体布局、CRC 覆盖范围、AES-CFB 流状态、随机 IV、过期时间尾缀、全量写回的占位 varint。见 §10.2。
3. Rust 侧不需要模仿 C++ 的对象模型。推荐：
   - `mmkv-platform`：**纯契约 crate**，定义 `Platform` / `MappedRegion` / `AppLifecycle` / 目录 / 保护 / RNG。
   - `mmkv-platform-posix`：POSIX 公共实现（mmap、`flock`、`ftruncate`、`msync`）。
   - `mmkv-platform-macos` / `-ios` / `-linux` / `-android` / `-windows`：各自实现平台策略。
   - `mmkv-core`：与平台无关的引擎（格式、编解码、加密、store、过期、备份）。
   - `mmkv`：门面 crate，按 `cfg(target_os)` 选择平台实现，提供 `Mmkv`、全局初始化、实例注册表。
   - `mmkv-ffi`（后续）：C ABI，供 Swift/Kotlin 复用。
4. Rust API 不逐字翻译 C++ 重载，而是：**写操作 `Result<()>`、读操作 `Option<T>`、默认值变体、零拷贝 `with_bytes/with_str` 闭包**；同时保留 `get_*_opt` 表达 `hasValue`。
5. 默认目录严格沿用 MMKV：iOS/macOS/watchOS = `NSDocumentDirectory/mmkv`，tvOS = `NSCachesDirectory/mmkv`，Android = `filesDir/mmkv`，App Group = `<group>/mmkv`。**iOS 上不需要 Swift 也能拿到这些路径**（Foundation API 由 `objc2-foundation` 直接调用），不提供 Swift shim。
6. 正确性验证必须做 **C++ 差分测试**。当前实现方式是：由 C++ 生成 golden fixtures 提交到仓库，Rust 测试直接读取；移除 C++ bridge 之前已完成一次 Rust→C++ 双向验证。没有这一层，字节级兼容只能靠猜。

---

## 1. 兼容性契约

### 1.1 必须保持一致（hard contract）

| 项 | 约束 |
|---|---|
| 数据文件名 | `<root>/<encodeFilePath(id)>` |
| meta 文件名 | 数据文件路径 + `.crc` |
| 特殊字符 id | 含 `\ / : * ? " < > \|` 时：`<root>/specialCharacter/<md5(id)>`（小写 hex） |
| 默认实例 id | `mmkv.default` |
| 数据文件头 | offset 0：`u32 LE` 旧式 actualSize；offset 4 起为记录流 |
| 记录编码 | `varint32(keyLen) + key + varint32(valueLen) + value` |
| data-holder | `valueLen = rawLen + varint_size(rawLen)`，payload = `varint(rawLen) + raw` |
| 删除 | 追加一条 `valueLen == 0` 的记录 |
| 流首 | 一个 4 字节的伪 map-size varint（解码时丢弃） |
| meta 结构 | 112 字节，字段偏移与字节序固定（§2.3） |
| CRC | zlib CRC-32（poly 0xEDB88320，init/final xor 0xFFFFFFFF），只覆盖 payload/ciphertext |
| 加密 | AES-CFB128，无 padding，整段 payload 一条 CFB 流 |
| IV | meta version ≥ 2 用 `m_vector`；否则用 `key[0..16]` |
| 密钥 | 原始字节截断/零填充到 16 或 32 字节，无 KDF |
| 过期 | 每个 value 尾部追加 `u32 LE` Unix 秒；0 = never |
| 全量写回 | 新随机 IV、重排 offset、`m_sequence++`、`lastConfirmed = (size, crc)` |
| 多进程锁 | 锁在 `.crc` 文件 fd 上（POSIX `flock`），不额外建锁文件 |
| 实例注册键 | `md5(absolute_dir + "/" + id)` |

### 1.2 允许不同（soft areas）

- 语言/线程模型：C++ 递归 `pthread_mutex` → Rust `parking_lot::Mutex`（用内部 `*_locked` 方法避免重入）。
- 内存中的 holder 表示：C++ 的 union/`paddedValue` → Rust enum + `SmallVec`/`Box<[u8]>`；不落盘。
- 全量写回的 4 字节占位 varint 数值随机；只要求长度和可解析性，不要求字节相同。
- HashMap 迭代顺序：不落盘影响（全量写回按 offset 排序，见 §2.4）。
- Apple 的 `NSString/NSDate/NSCoding` ObjC 对象 API：Rust 公开 API 只覆盖原始类型、字符串、字节、向量；`NSDate` 按 `f64` 秒处理；`NSCoding` 交由后续 `mmkv-apple` 桥接层。
- Android ashmem、跨进程 ContentProvider、Flutter/Dart/Python 绑定：后续阶段，非首期目标。

### 1.3 许可

MMKV 为 BSD-3-Clause。移植代码必须保留 Tencent 版权与许可声明，新增 `LICENSE`/`NOTICE`，并在文件头注明“port of MMKV v2.4.3”。

---

## 2. MMKV 机制拆解

### 2.1 路径与文件

```
<root>/mmkv.default           数据文件（mmap）
<root>/mmkv.default.crc       meta/lock 文件（mmap，至少 1 页）
<root>/specialCharacter/<md5(id)>      特殊字符 id 的数据文件
<root>/specialCharacter/<md5(id)>.crc  特殊字符 id 的 meta 文件
```

- 页面大小运行时获取：`DEFAULT_MMAP_SIZE = getpagesize()`（Apple Silicon 常见 16384，x86 4096）。
- 新文件：数据文件 `max(page, roundUp(expectedCapacity, page))`；meta 文件 1 页。
- 所有可写文件的 `ftruncate` 目标都是页的整数倍；扩容后显式写零（不是 `fallocate`）。
- 文件权限 `0700`，`O_CLOEXEC`。
- 单进程：数据/meta fd 在 mmap 后可关闭（mayfly fd）；多进程：meta fd 必须常驻，因为 `flock` 锁在它上面。

### 2.2 数据文件格式

```
offset 0                u32 LE  legacy actualSize（version < 3 时有效；不参与 CRC）
offset 4                payload（version ≥ 3 时长度由 meta.m_actualSize 决定）
offset 4 + actualSize   ... 到文件尾为未使用/陈旧数据
```

payload：

```
varint32  fake_map_size        // 通常 4 字节，值随机，解析时丢弃
repeat:
    varint32 key_len
    key[key_len]               // UTF-8
    varint32 value_len
    value[value_len]
```

- 普通值：`value` 就是序列化后的值（bool=1 字节、varint、fixed32/64、长度前缀字符串/向量等）。
- data-holder 值（字符串/bytes）：`value_len = rawLen + varint_size(rawLen)`，`value = varint(rawLen) + raw`。这是为了 `getValueSize(actualSize=true)` 和零拷贝。
- 删除：`value_len == 0`；加载时后者覆盖前者，最终从字典移除。
- key 长度上限 `KeySizeLimit = UINT16_MAX - 4 = 65531`。
- 全量写回时把活跃记录**按 offset 升序**密集重排；`fake_map_size` 占位通常 4 字节，若旧文件最小 offset ≤ 5 且 ≠ 4，则沿用该宽度（legacy 兼容）。

### 2.3 Meta（`.crc`）文件

`MMKVMetaInfo`，112 字节，自然对齐，小端：

| offset | size | 字段 |
|---|---|---|
| 0 | 4 | `m_crcDigest: u32` |
| 4 | 4 | `m_version: u32` |
| 8 | 4 | `m_sequence: u32`（全量写回计数） |
| 12 | 16 | `m_vector[16]`（AES IV） |
| 28 | 4 | `m_actualSize: u32` |
| 32 | 4 | `lastActualSize: u32` |
| 36 | 4 | `lastCRCDigest: u32` |
| 40 | 64 | `_reserved[16]` |
| 104 | 8 | `m_flags: u64`（bit0 = EnableKeyExpire） |

版本：

```
0 Default
1 Sequence
2 RandomIV
3 ActualSize
4 Flag          // 现代 MMKV 首次写入后至少为 4
5 Preserved
6 Next
7 Holder        // >= 7 视为异常，回落到 3
```

写策略：

- 普通追加：只写 `m_crcDigest` + `m_actualSize`（保持 version/IV/sequence 不变）。
- 全量写回 / clear / trim / reKey / 恢复：整结构写入，`m_sequence++`，更新 `lastConfirmed`。
- 加密全量写回：写入新随机 IV，version ≥ 2。
- `m_flags` 只有在 version ≥ 4 时才有效；version < 4 时清零。

### 2.4 加载 / 写入 / 压缩

**加载**（`loadFromFile`）：

1. 读 meta，必要时修复/降级。
2. 校验数据文件大小与 CRC；失败时尝试 `lastConfirmed`；仍失败则按 handler 的 `RecoverStrategy`（Discard/Recover）处理。
3. 顺序解码 payload → `HashMap<String, KeyValueHolder>`；重复 key 后者胜出；零长度值删除。
4. 设置输出游标到 `actualSize`。
5. 加密且 meta version < 2：触发一次全量写回升级随机 IV。

**追加**（`doAppendDataWithKey`）：

1. 计算编码后大小。
2. 保证空间（必要时全量写回/扩容）。
3. 写入 key/value 到 mmap 尾部。
4. 加密：对写入区间原地 CFB 加密。
5. 更新 CRC + actualSize（只写 meta 的两个字段）。
6. 更新内存 holder（offset/keySize/valueSize）。

**覆盖写**（`doOverrideDataWithKey`）：

- 仅单进程、只有一个 key、且不加密、文件空间足够时，从 offset 0 重写；否则退化为追加。
- v2.4.3 明确：**加密路径永不覆盖写**（避免 IV 复用），一律追加，靠全量写回轮换 IV。

**全量写回**（`fullWriteback`）：

1. 逐个 key 收集/序列化（加密时只搬运 offset holder + 新编码 Direct/Memory holder）。
2. 生成新随机 IV（如加密）。
3. 写 4 字节占位 varint。
4. 按旧 offset 升序 `memmove` 连续区段；加密时用旧 holder 的 CFB status 解密、再用新 IV 加密。
5. 更新所有 holder 的 offset（加密还更新 cryptStatus）。
6. `m_actualSize = total`，重算 CRC，meta 写 `m_sequence++` + `lastConfirmed` + IV。

**扩容**：`lenNeeded = sizeOfDic + 4 + newSize`；若 `lenNeeded >= fileSize` 或（需要 sync 且）预计未来用量超过文件，则文件大小翻倍，直到容纳；然后 `ftruncate`（页对齐）+ 零填充 + 重新 mmap，再全量写回。

**trim**：先全量写回；文件大小反复减半直到 `<= (actualSize + 4) * 2`，但不低于 `expectedCapacity`。

**clearAll**：可选先 truncate 回 `expectedCapacity`，加密时换新 IV，`writeActualSize(0, 0, iv, IncreaseSequence)`，msync meta，重载。

### 2.5 多进程

- 模式位：`SINGLE_PROCESS = 1<<0`、`MULTI_PROCESS = 1<<1`、`READ_ONLY = 1<<5`（Android 另有 `1<<2/3/4`）。
- 锁：所有实例操作锁 `<id>.crc` 的 fd。
  - POSIX/macOS/iOS：`flock(LOCK_SH/LOCK_EX)`，非阻塞用 `LOCK_NB`。
  - Android 普通文件：`flock`；ashmem：`fcntl(F_SETLK/F_SETLKW)`；进程模式检查/迁移：`F_OFD_SETLK` 指定 byte range。
- 读写路径：读加共享锁，写加独占锁；C++ 的 `InterProcessLock` 维护 shared/exclusive 递归计数，并支持“持共享锁升级独占锁时先释放再重试”。
- 变更检测（`checkContentChanged` / 每次操作前 `checkLoadData`）：
  1. 比较 meta `m_sequence`；不同 → 清缓存 + 全量重载 + `onContentChanged`。
  2. 否则比较 `m_crcDigest` / `m_actualSize`；不同 → `partialLoadFromFile`（增量 CRC + 只解码新增区间）。
  3. 增量校验失败 → 回退全量重载。
- 临时锁文件：**没有**。不要引入 `.lock` 文件，否则会改变文件布局语义（多余文件）且与 C++ 实现不互认。

### 2.6 加密

- 算法：**AES-CFB128**（16 字节段），加密/解密共用同一条 CFB 状态 `(ivec[16], num)`；反馈字节始终是密文。
- 无 padding；密文长度 == 明文长度。
- 密钥：`cryptKey[0..min(len,16/32)]` + 零填充；AES-128 或 AES-256 由调用方 `aes256` 决定，**磁盘上没有标记**。
- IV：每次全量写回生成 16 字节随机数（`/dev/urandom`，Windows `CryptGenRandom`），写入 `meta.m_vector`。
- 整段 payload 是一条 CFB 流，从 offset 4 开始；`num = 全局偏移 % 16` 恒成立。
- 内存 holder：
  - `Direct`：明文 ≤ 27 字节，内联。
  - `Memory`：明文 28..=256 字节，堆上缓存。
  - `Offset`：`value_len > 256`，值留在 mmap 中；需要 `offset`、`pbKeyValueSize`（两个 varint 长度）、`keySize`、`valueSize`、以及记录起点的 `AESCryptStatus { num: u8, ivec: [u8;16] }`（17 字节，仅内存）。
- 读取 offset holder：从记录起点状态克隆 CFB，先解 `pbKeyValueSize + keySize` 字节到值起点，再解 `valueSize` 字节。
- C++ 的 `statusBeforeDecrypt` 用于“预读后回滚”；Rust 侧**顺序解码、不预读**即可完全避免实现反向 AES，只需正向块加密。
- 删除仍是追加零长度记录；加密下更新一律追加，全量写回时才轮换 IV。

### 2.7 过期与 compare-before-set

- 全局开关存于 `meta.m_flags` bit0；`enableAutoKeyExpire(seconds)` 会把所有现有值重写为 `value + u32 LE expireAt`。
- 读取时：若 `value.len() < 4` 视为非法；否则取尾部 4 字节；`time != 0 && time <= now` → 删除该 key 并返回空。
- `filterExpiredKeys` 在写满/全量写回前清理。
- `compareBeforeSet`：明文、非过期模式下，set 前比较旧值；相同则直接返回成功，避免文件膨胀。加密或过期模式下禁用。

### 2.8 平台差异（与磁盘格式无关）

| 平台 | 默认根目录 | 特殊处理 |
|---|---|---|
| iOS / macOS / watchOS / visionOS | `NSSearchPathForDirectoriesInDomains(NSDocumentDirectory, NSUserDomainMask, YES)[0]/mmkv` | iOS 可写文件设置 `NSFileProtectionCompleteUntilFirstUserAuthentication`；后台状态只影响空闲实例清理 |
| tvOS | `NSCachesDirectory/mmkv` | 无备份 |
| iOS App Group | `<groupDir>/mmkv` | 多进程共享 |
| Android | `Context.getFilesDir()/mmkv` | cacheDir 作临时目录；ashmem 可选 |
| Linux | 调用方显式指定（demo 用 `/tmp/mmkv`） | 无默认 |
| Windows | 调用方显式指定 | 宽字符路径、`LockFileEx` |

MMKV 本身**不做** iCloud 备份排除；Rust 移植默认也不做，提供 opt-in。

---

## 3. Rust 架构

### 3.1 Crate 依赖图

```
mmkv-rs                  门面：init/init_with、Mmkv、Namespace，按 cfg 选平台
 ├── mmkv-core            引擎：格式/编解码/加密/store/过期/备份/锁/自动清理
 │    └── mmkv-platform   契约：Platform/MappedRegion/AppLifecycle/FileLockKind
 ├── mmkv-platform-macos  macOS/tvOS/watchOS/visionOS（objc2）
 ├── mmkv-platform-ios    iOS（objc2、文件保护、生命周期）
 ├── mmkv-platform-linux  Linux（POSIX + sendfile）
 ├── mmkv-platform-android Android（JNI 路径、进程模式、legacy 迁移、ashmem）
 └── mmkv-platform-windows Windows（CreateFileMappingW/LockFileEx）
        └── mmkv-platform-posix（macOS/iOS/Linux/Android 共用底层）
```

- `mmkv-core` 只依赖 `mmkv-platform` 的 trait，不直接调用 `std::fs` 之外的 OS API。
- `mmkv` 用 `cfg(target_os)` 选择平台 crate，并 `pub type PlatformImpl = ...`；也允许用户直接依赖 `mmkv-core` + 自定义 `Platform`。
- 语言绑定与 CI 不在本项目范围内（按用户决策）。

### 3.2 `mmkv-platform` 契约

目标：**dyn-safe**，便于运行时替换与测试 mock。

```rust
pub enum LockMode { Shared, Exclusive }
pub enum LockOutcome { Acquired, WouldBlock }
pub enum SyncMode { Sync, Async }
pub enum AppState { Foreground, Background }

pub trait MappedRegion: Send + Sync {
    fn size(&self) -> usize;
    fn as_slice(&self) -> &[u8];
    fn as_mut_slice(&mut self) -> &mut [u8];
    /// 页对齐扩容/缩容；扩容区间必须显式零填充；失败回滚旧大小。
    fn resize(&mut self, new_size: usize) -> std::io::Result<()>;
    fn sync(&self, mode: SyncMode) -> std::io::Result<()>;
    fn lock(&self, mode: LockMode, wait: bool) -> std::io::Result<LockOutcome>;
    fn unlock(&self, mode: LockMode) -> std::io::Result<()>;
    /// 多进程/备份场景需要常驻 fd；单进程 mayfly 可返回 None。
    fn file(&self) -> Option<&std::fs::File>;
}

pub trait Platform: Send + Sync {
    fn name(&self) -> &'static str;

    /// 默认根目录，已包含末尾 `mmkv`。
    fn default_root_dir(&self) -> std::io::Result<std::path::PathBuf>;

    /// App Group / 共享目录；group 为平台标识（iOS: group id）。
    fn shared_root_dir(&self, group: Option<&str>) -> std::io::Result<std::path::PathBuf>;

    fn temp_dir(&self) -> std::io::Result<std::path::PathBuf>;
    fn page_size(&self) -> usize;

    fn open_region(
        &self,
        path: &std::path::Path,
        expected_size: usize,
        read_only: bool,
        mayfly_fd: bool,
    ) -> std::io::Result<Box<dyn MappedRegion>>;

    /// iOS: NSFileProtection…；其他平台默认 no-op。
    fn protect_file(&self, _path: &std::path::Path) -> std::io::Result<()> { Ok(()) }
    /// 可选：排除 iCloud 备份。
    fn exclude_from_backup(&self, _path: &std::path::Path) -> std::io::Result<()> { Ok(()) }

    fn random_bytes(&self, out: &mut [u8]) -> std::io::Result<()>;
    fn now_unix_seconds(&self) -> u32;

    fn app_lifecycle(&self) -> &dyn AppLifecycle;
}

pub trait AppLifecycle: Send + Sync {
    fn state(&self) -> AppState;
    /// 注册回调；返回取消句柄。用于 iOS 内存警告/后台通知。
    fn subscribe(&self, cb: Box<dyn Fn(AppEvent) + Send + Sync>) -> Box<dyn FnOnce()>;
}
```

- `mmkv-platform-posix` 提供 `PosixMappedRegion`：`memmap2::MmapMut` + `File`，`resize` 走 `ftruncate` + 零填充 + remap；`lock/unlock` 走 `libc::flock`；`sync` 走 `msync`。
- Android ashmem 后续在 `mmkv-platform-android` 提供另一种 `MappedRegion`（固定大小、`fcntl` 锁、无 fd 返回）。
- 测试用 `InMemoryRegion`（`mmkv-platform` 内 `#[cfg(test)]` 或 `testing` feature）可以在无文件系统情况下跑核心逻辑。

### 3.3 `mmkv-core` 模块

```
crates/mmkv-core/src/
├── lib.rs
├── error.rs            Error / Result / RecoverStrategy
├── config.rs           StoreConfig / Mode / Expiration / InitOptions
├── log.rs              级别、全局 handler、宏
├── handler.rs          Handler trait（CRC/长度错误、内容变更、加载成功、日志）
├── crc32.rs            zlib CRC-32（crc32fast 或自实现表）
├── meta.rs             112 字节 meta 的 LE 读写 + version/flags
├── md5.rs              仅用于文件名（md-5 crate），带注释“非安全用途”
├── path.rs             encodeFilePath / mappedKVPathWithID / crcPath / registry key
├── buf.rs              小缓冲优化（SmallVec 可选项）
├── codec/
│   ├── mod.rs
│   ├── varint.rs       varint32/64、zigzag 无关、size 计算
│   ├── input.rs        CodedInputData（边界检查、Read 错误）
│   ├── output.rs       CodedOutputData（游标、容量检查）
│   └── mini_pb.rs      map/string/vector/primitive 编解码
├── kv/
│   ├── mod.rs
│   ├── holder.rs       KeyValueHolder / KeyValueHolderCrypt
│   └── dictionary.rs   HashMap<Box<str>, Holder>
├── crypt/
│   ├── mod.rs
│   ├── aes.rs          转发到 `aes` crate 的 BlockEncrypt
│   ├── cfb.rs          CFB-128 状态机 + AesStatus
│   └── input.rs        加密流顺序解码（无预读、无回滚）
├── memory.rs           MemoryFile：页对齐、零填充、remap、msync、mayfly
├── lock.rs             递归 shared/exclusive 文件锁（对应 InterProcessLock/FileLock）
├── store/
│   ├── mod.rs          Store：字段、构造、close
│   ├── load.rs         loadFromFile / partialLoad / checkDataValid / 恢复
│   ├── read.rs         get_* / value_size / write_value
│   ├── write.rs        setDataForKey / append / override / remove
│   ├── compact.rs      fullWriteback / memmoveDictionary / ensureMemorySize
│   ├── expire.rs       过期/过滤/迁移
│   ├── admin.rs        clear / trim / sync / clearMemoryCache / import
│   └── backup.rs       backup/restore one/all
├── registry.rs         全局实例表（Weak）+ init + shutdown
├── namespace.rs        Namespace
└── tests/              单元 + 格式 fixtures
```

**关键类型**

```rust
pub struct Store {
    id: String,
    mode: Mode,
    path: PathBuf,
    crc_path: PathBuf,
    state: parking_lot::Mutex<State>,
    data: Box<dyn MappedRegion>,   // 不可 Clone
    meta: Box<dyn MappedRegion>,
    meta_info: MMKVMetaInfo,
    lock: FileLock,
    crypter: Option<AesCrypt>,
    platform: Arc<dyn Platform>,
}

struct State {
    dict: Dictionary,          // plain 或 crypt 两种 holder
    actual_size: usize,
    output_position: usize,
    need_load: bool,
    has_full_writeback: bool,
    crc_digest: u32,
    expected_capacity: usize,
    expiration: ExpirationState,
    closed: bool,
}
```

`Mmkv` 是 `Arc<Store>` 的轻量句柄（Clone 共享）。`close(self)` 从 registry 移除并置 `closed`；其他克隆之后的调用返回 `Error::Closed`（Rust 无法像 C++ 那样悬空指针，语义上更安全）。

### 3.4 并发与内存映射

- **锁顺序**：`State` mutex → 文件锁（共享/独占）。与 C++ 的 `m_lock → m_sharedProcessLock` 一致，避免死锁。
- `FileLock` 维护 shared/exclusive 递归计数，支持：
  - 持共享锁再做共享：计数 +1；
  - 持共享锁升级独占：先尝试非阻塞独占，失败则释放共享、阻塞独占，失败再恢复共享；
  - 解锁独占但仍有共享：降级为共享。
- getter：`state.lock()` → `check_load_data_locked()` → 共享文件锁 → 读/解密 → 复制为 `String`/`Vec<u8>`（或闭包内借用）。
- setter：`state.lock()` → `check_load_data_locked()` → 独占文件锁 → 追加/压缩 → 更新 meta。
- 不向用户暴露跨 remap 的引用。零拷贝用 `with_bytes(key, f)` / `with_str(key, f)`，闭包执行期间持锁，天然安全。
- `Sync`：`Mutex` + `MappedRegion: Send + Sync`；`Store` 可 `Arc` 共享。
- 公共 `lock()/try_lock()` 返回 RAII guard（独占进程锁），内部方法通过递归计数避免二次加锁死锁。

### 3.5 错误、日志与回调

```rust
pub enum Error {
    NotInitialized,
    InvalidId,
    Closed,
    ReadOnly,
    FileNotValid,
    Io(std::io::Error),
    TooLarge { key: String, size: usize },
    ItemSizeLimit { size: usize, limit: u32 },
    Corrupted(RecoverStrategy),
    LockBusy,
}
```

- 读失败（截断、解码错误、CRC 不符）不打 `Err` 给用户，按 MMKV 语义记日志并当作“不存在”；`get_*_opt` 返回 `None`。
- 写失败返回 `Result`，并携带具体原因（只读、超限、IO）。
- `Handler` 全局注册（`Arc<dyn Handler>`），提供 `RecoverStrategy { Discard, Recover }`；`StoreConfig.recover` 覆盖全局。
- 日志级别 `Debug/Info/Warning/Error/None`；默认 `Info`。可选 `log` feature 把日志转发到 `log` crate。

---

## 4. Rust 公开 API

### 4.1 初始化与打开

```rust
pub fn init(options: InitOptions<'_>) -> Result<PathBuf, Error>;
pub fn root_dir() -> PathBuf;          // 首次 init 的 root（原始字符串）

pub fn default_store(config: StoreConfig<'_>) -> Result<Mmkv, Error>;
pub fn open(id: impl Into<String>, config: StoreConfig<'_>) -> Result<Mmkv, Error>;

pub fn namespace(root: impl Into<PathBuf>) -> Namespace;
pub fn default_namespace() -> Namespace;
```

```rust
pub struct InitOptions<'a> {
    pub root_dir: Option<&'a Path>,
    pub group_dir: Option<&'a Path>,   // iOS App Group
    pub log_level: Level,
    pub handler: Option<Arc<dyn Handler>>,
}

pub struct StoreConfig<'a> {
    pub mode: Mode,                    // SINGLE_PROCESS / MULTI_PROCESS / READ_ONLY
    pub crypt_key: Option<&'a CryptKey>,
    pub aes256: bool,
    pub root: Option<&'a Path>,
    pub expected_capacity: usize,
    pub expiration: Option<Expiration>,
    pub compare_before_set: bool,
    pub recover: Option<RecoverStrategy>,
    pub item_size_limit: u32,
}
```

`StoreConfig` 提供 builder 方法，避免一堆 `..Default::default()`。

### 4.2 读写

| 值类型 | 写 | 读（默认值） | 读（Option） |
|---|---|---|---|
| bool | `put_bool(key, v)` | `get_bool(key, default)` | `get_bool_opt(key)` |
| i32 | `put_i32` | `get_i32` | `get_i32_opt` |
| u32 | `put_u32` | `get_u32` | `get_u32_opt` |
| i64 | `put_i64` | `get_i64` | `get_i64_opt` |
| u64 | `put_u64` | `get_u64` | `get_u64_opt` |
| f32 | `put_f32` | `get_f32` | `get_f32_opt` |
| f64 | `put_f64` | `get_f64` | `get_f64_opt` |
| &str | `put_str` | `get_str_or(key, default)` | `get_str(key)` |
| &[u8] | `put_bytes` | `get_bytes_or` | `get_bytes(key)` |
| &[String] | `put_strings` | `get_strings_or` | `get_strings(key)` |
| Vec<bool> | `put_bool_vec` | — | `get_bool_vec(key)` |
| Vec<i32> | `put_i32_vec` | — | `get_i32_vec(key)` |
| Vec<u32> | `put_u32_vec` | — | `get_u32_vec(key)` |
| Vec<i64> | `put_i64_vec` | — | `get_i64_vec(key)` |
| Vec<u64> | `put_u64_vec` | — | `get_u64_vec(key)` |
| Vec<f32> | `put_f32_vec` | — | `get_f32_vec(key)` |
| Vec<f64> | `put_f64_vec` | — | `get_f64_vec(key)` |

所有 `put_*` 都有 `_expiring(key, v, seconds)` 变体（对应 C++ 的 `expireDuration`）。

零拷贝：

```rust
pub fn with_str<R>(&self, key: &str, f: impl FnOnce(Option<&str>) -> R) -> R;
pub fn with_bytes<R>(&self, key: &str, f: impl FnOnce(Option<&[u8]>) -> R) -> R;
```

其他：

```rust
pub fn contains(&self, key: &str) -> bool;
pub fn remove(&self, key: &str) -> Result<bool, Error>;
pub fn remove_many(&self, keys: &[&str]) -> Result<(), Error>;
pub fn value_size(&self, key: &str, actual: bool) -> usize;
pub fn write_value(&self, key: &str, out: &mut [u8]) -> Result<usize, Error>;
```

### 4.3 管理与元信息

```rust
pub fn all_keys(&self) -> Vec<String>;
pub fn all_keys_non_expired(&self) -> Vec<String>;
pub fn count(&self) -> usize;
pub fn count_non_expired(&self) -> usize;
pub fn total_size(&self) -> usize;
pub fn actual_size(&self) -> usize;

pub fn clear(&self);
pub fn clear_keep_space(&self);
pub fn trim(&self);
pub fn clear_memory_cache(&self, keep_space: bool);
pub fn sync(&self);
pub fn sync_async(&self);
pub fn import_from(&self, src: &Mmkv) -> Result<usize, Error>;
pub fn check_content_changed(&self);

pub fn enable_expiration(&self, seconds: u32) -> Result<(), Error>;
pub fn disable_expiration(&self) -> Result<(), Error>;
pub fn enable_compare_before_set(&self) -> Result<(), Error>;
pub fn disable_compare_before_set(&self) -> Result<(), Error>;

pub fn rekey(&self, key: Option<&CryptKey>, aes256: bool) -> Result<(), Error>;
pub fn crypt_key(&self) -> Option<CryptKey>;
pub fn reset_crypt_key(&self, key: Option<&CryptKey>);

pub fn lock(&self) -> LockGuard<'_>;
pub fn try_lock(&self) -> Option<LockGuard<'_>>;

pub fn is_multi_process(&self) -> bool;
pub fn is_read_only(&self) -> bool;
pub fn is_expiration_enabled(&self) -> bool;
pub fn is_encryption_enabled(&self) -> bool;
pub fn is_compare_before_set_enabled(&self) -> bool;
```

### 4.4 静态工具

```rust
pub fn page_size() -> usize;
pub fn version() -> &'static str;          // "v2.4.3"
pub fn is_file_valid(id: &str, root: Option<&Path>) -> bool;
pub fn exists(id: &str, root: Option<&Path>) -> bool;
pub fn remove_storage(id: &str, root: Option<&Path>) -> Result<bool, Error>;
pub fn backup_one(id: &str, dst: &Path, src_root: Option<&Path>) -> bool;
pub fn restore_one(id: &str, src: &Path, dst_root: Option<&Path>) -> bool;
pub fn backup_all(dst: &Path, src_root: Option<&Path>) -> usize;
pub fn restore_all(src: &Path, dst_root: Option<&Path>) -> usize;
pub fn set_log_level(level: Level);
pub fn set_handler(handler: Option<Arc<dyn Handler>>);
pub fn shutdown();
```

### 4.5 C++ → Rust 命名映射（节选）

| C++ | Rust |
|---|---|
| `initializeMMKV` | `init` |
| `defaultMMKV` / `mmkvWithID` | `default_store` / `open` |
| `getBool(key, default, hasValue)` | `get_bool(key, default)` / `get_bool_opt` |
| `set(value, key, expireDuration)` | `put_*_expiring` |
| `getString(key, out)` | `get_str` / `get_str_or` / `with_str` |
| `getBytes` | `get_bytes` / `with_bytes` |
| `getVector` | `get_strings` / `get_*_vec` |
| `allKeys(filterExpire)` | `all_keys` / `all_keys_non_expired` |
| `clearAll(keepSpace)` | `clear` / `clear_keep_space` |
| `sync(SyncFlag)` | `sync` / `sync_async` |
| `lock/unlock/try_lock` | `lock()->Guard` / `try_lock()` |
| `reKey` | `rekey` |
| `enableAutoKeyExpire` | `enable_expiration` |
| `removeStorage` | `remove_storage` |
| `checkExist` | `exists` |
| `onExit` | `shutdown` |
| `NameSpace` | `Namespace` |

---

## 5. 平台实现

### 5.1 macOS（`mmkv-platform-macos`）

- 根目录：`NSSearchPathForDirectoriesInDomains(.documentDirectory, .userDomainMask, true)[0] + "/mmkv"`，通过 `objc2-foundation` 调用；沙盒下自动落在容器内。
- 创建目录：`NSFileManager.createDirectoryAtPath:withIntermediateDirectories:YES`（与 C++ 一致，避免 POSIX `stat` 隐私问题）。
- mmap/lock：复用 `mmkv-platform-posix`。
- 生命周期：macOS 无 iOS 后台/内存警告语义，`AppLifecycle` 返回 `Foreground`，订阅为空实现。
- 文件保护：no-op（MMKV 只在 `MMKV_IOS` 下设置）。

### 5.2 iOS（`mmkv-platform-ios`）

- 根目录：`NSDocumentDirectory/mmkv`（**不是** `Library/Caches`）；tvOS 单独 `NSCachesDirectory/mmkv`。
- App Group：`FileManager.containerURLForSecurityApplicationGroupIdentifier(group)` + `/mmkv`。该 API 通过 `objc2-foundation` 可直接调用。
- 文件保护：对可写的数据文件和 `.crc` 设置 `NSFileProtectionCompleteUntilFirstUserAuthentication`（`NSFileManager.setAttributes:`）。只读实例不改属性。
- 生命周期：订阅 `UIApplicationDidEnterBackgroundNotification` / `DidBecomeActiveNotification`，维护 `is_in_background`；内存警告 `UIApplicationDidReceiveMemoryWarningNotification` → 对所有实例 `clear_memory_cache(false)`。
- App Extension：检测 `Bundle.main.bundlePath.hasSuffix(".appex")`，扩展下不订阅 UIApplication 通知（默认目录已是扩展容器）。
- **关于 Swift**：最终方案明确使用 `objc2`/`objc2-foundation` 直接调用 Foundation，**不使用 Swift shim**，也不引入 `swiftc`/Objective-C++ 构建步骤。上述路径、文件保护、App Group 解析都由 Rust 侧完成。
- 编译目标：`aarch64-apple-ios`、`aarch64-apple-ios-sim`；最低 iOS 15（跟随 MMKV v2.4.3 之后的计划；当前可用 13）。

### 5.3 Linux（`mmkv-platform-linux`）

- 无隐式默认目录：`init` 必须给 `root_dir`；门面可提供 `dirs` feature 使用 `XDG_DATA_HOME`，但**不作为默认行为**（保持 1:1）。
- `mmkv-platform-posix`：`flock`、`ftruncate`、`msync`、`getpagesize`、`/dev/urandom`。
- 原子 rename：优先 `renameat2(RENAME_EXCHANGE)`，回落 `rename`（与 C++ 一致）。
- 备份复制：`copy_file_range`/`sendfile` 可作为优化，语义与 C++ 的 `sendfile` 一致。

### 5.4 Android（`mmkv-platform-android`，阶段 5+）

- 根目录：`Context.getFilesDir()/mmkv`；`cacheDir` 作为临时目录；需 JNI/`ndk-context` 获取 `Context`（或由调用方传入路径）。
- 首期只做普通文件（`flock`），ashmem 后置：
  - ashmem 固定大小、不可 `ftruncate`；
  - 数据锁 `fcntl(F_SETLK)`，普通文件 `flock`；
  - 进程模式检查用 `F_OFD_SETLK` 的 byte range（offset 1/2），迁移锁 offset 2/3；
  - 跨进程 fd 传递需要 ContentProvider/JNI，属于绑定层。
- 多进程检测包含 `MULTI_PROCESS` 位与 ashmem 位。

### 5.5 Windows（`mmkv-platform-windows`，阶段 5+）

- 路径宽字符 `PathBuf`（Rust 原生 UTF-16 友好）；`LockFileEx` 模拟共享/独占；`CreateFileMapping`/`MapViewOfFile`；`CryptGenRandom` 或 `BCryptGenRandom`。
- 文件大小、页对齐、零填充语义与 POSIX 一致。

---

## 6. 分阶段计划

### P0 — 工作区与契约（2 人日）

任务：
- 把 `crates/mmkv` 从 binary 改为 library（保留 `examples/`）。
- 新增 `mmkv-platform-posix`、`mmkv-platform-ios`、`mmkv-platform-linux` crate 骨架（先 `unimplemented!` 的 `Platform`，能编译）。
- 定义 `mmkv-platform` 的 trait、错误类型、`MappedRegion`、`AppLifecycle`。
- 在 `mmkv-core` 建立模块骨架、`log`/`handler`/`config`/`error`。
- `cargo check --workspace`、`cargo fmt`、`clippy` 通过。

验收：workspace 可编译；平台 trait 有 mock 实现并能创建内存 region；无 `todo!()` 进入公开 API。

### P1 — 只读明文兼容（4–6 人日）

任务：
- `meta.rs`、`crc32.rs`、`varint.rs`、`input.rs`、`mini_pb.rs` 解码、`holder.rs`（明文）。
- `MappedRegion` 只读映射、`MemoryFile` 只读。
- `Store::load`：`checkDataValid`（CRC/长度/lastConfirmed）、顺序解码、`all_keys/count/get_*`。
- `is_file_valid` / `exists` / `open` 的只读路径。
- C++ 参考 harness + 明文 fixtures（§7）。

验收：
- 读取 C++ 生成的 fixtures：bool/i32/u32/i64/u64/f32/f64/string/bytes/string-array 全部一致。
- 删除中间态、重复 key、全量写回后的文件读取正确。
- `is_file_valid` 与 C++ 一致（含篡改 CRC 的负例）。

### P2 — 明文写入（6–9 人日）

任务：
- `output.rs`、追加写、删除 tombstone、holder 计算。
- `writeActualSize` 等价逻辑、meta version/sequence/CRC。
- `ensureMemorySize` + `expandAndWriteBack` + `fullWriteback`（明文 `memmoveDictionary`）。
- `trim`、`clearAll`、`sync`、`clearMemoryCache`、`import_from`。
- `compare_before_set`。
- 多进程共享/独占锁（`lock.rs`），内容变更检测与 `partialLoadFromFile`。
- 备份/恢复、`remove_storage`、`specialCharacter` 路径。

验收：
- Rust 写 → C++ 读，值/计数/actualSize/CRC 全一致。
- C++ 写 → Rust 追加 → C++ 再读，全一致。
- 文件增长、全量写回、trim 后 C++ `isFileValid` 为 true。
- 双进程并发追加压力测试通过（无丢写、CRC 一致）。

### P3 — 加密（5–7 人日）

任务：
- `aes.rs`（RustCrypto `aes`，AES-128/256）、`cfb.rs`（OpenSSL CFB128 状态机）。
- `AesCrypt`：密钥填充、IV、`status`、`clone_with_status`。
- `KeyValueHolderCrypt`（Direct/Memory/Offset）、加密流顺序解码。
- 加密追加/读取/删除、全量写回的 `memmoveDictionary(crypt)`。
- `rekey`（明文↔密文、换 key、AES-128↔256）、`reset_crypt_key`。
- 随机 IV、legacy version<2 的 key-IV 兼容。

验收：
- AES-128/256 fixtures 双向读写一致；小值（≤27/≤256）与大值（>256，offset holder）都覆盖。
- 单 key 反复更新的 IV 复用回归测试（v2.4.3 修复点）。
- rekey 后 C++ 能读；C++ rekey 后 Rust 能读。
- legacy（version 1，无随机 IV）文件可读，写时升级为 version ≥ 2。

### P4 — 高级功能与恢复（6–9 人日）

任务：
- 过期：`enable_expiration`/`disable_expiration`、尾部时间戳、读取惰性删除、`filterExpiredKeys`、迁移。
- `item_size_limit`、`write_value`、`value_size`。
- 恢复策略：CRC 失败/长度错误、`lastConfirmed`、旧式 actualSize 降级兼容、Apple mmap 读错误处理（`EDEVERR/EILSEQ/EINVAL/ENXIO`）。
- `Handler` 回调、日志级别、`shutdown`、instance registry 与 `close` 语义。
- `Namespace` 全部方法、`backup_all/restore_all` 的 specialCharacter 处理。
- `page_size`、`version`、只读实例。

验收：
- 过期 key 在读取/枚举/计数/全量写回中行为与 C++ 一致。
- 篡改文件、截断文件、meta version≥7、旧版本文件均按 C++ 策略恢复。
- 备份/恢复往返后 C++ 可读。

### P5 — 平台矩阵与多进程加固（6–10 人日）

任务：
- macOS/iOS 平台 crate 完成：默认目录、App Group、文件保护、后台/内存警告。
- Linux 平台 crate；`renameat2`/`sendfile` 路径。
- iOS 模拟器 + 真机 smoke test；`cargo build --target aarch64-apple-ios`。
- 多进程：macOS 双进程测试、Linux 双进程测试；锁升级/降级、`try_lock`。
- Android 文件后端（可选 ashmem 原型）。
- 性能基线：启动解码、追加写、读取、全量写回。

验收：
- 各平台 `default_root_dir` 与 MMKV 完全一致（用 C++ 打印对比）。
- iOS 文件保护属性正确；后台状态只影响空闲清理。
- 双进程并发 30 分钟无死锁、无 CRC 错误。

### P6 — FFI 与生态（6–10 人日）

任务：
- `mmkv-ffi`：镜像 `Core/cbridge/MMKVBridge.h`，C ABI，`mmkv_free` 所有权规则。
- Swift Package wrapper（可选直接暴露 C ABI）。
- Android JNI wrapper（普通文件模式）。
- CI：macOS aarch64/x86_64、Linux x86_64、iOS 模拟器；`fmt`/`clippy`/`test`/fixtures。
- 文档：迁移指南、与 C++ API 对照、限制说明。

验收：Swift/Kotlin demo 可读写同一个 store，并与 C++ MMKV 互操作。

---

## 7. 测试与兼容性验证

### 7.1 C++ 参考实现 harness

在 `tools/mmkv-ref/` 放一个最小 C++ CLI（不进入 workspace）：

```text
mmkv-ref gen <dir> <scenario>   # 用 C++ 生成场景文件
mmkv-ref dump <dir> <id>        # 打印所有 key/value（类型化）
mmkv-ref verify <dir> <id>      # isFileValid + CRC + 枚举
```

构建：

```bash
cmake -S MMKV/Core -B target/mmkv-ref -DCMAKE_BUILD_TYPE=Release
cmake --build target/mmkv-ref -j
```

场景覆盖：空文件、各原始类型、字符串、bytes、字符串数组、数值数组、删除、重复 key、单 key 反复更新、全量写回、trim、过期、AES-128/256、legacy version、多进程追加。

### 7.2 Golden fixtures

- 把 harness 生成的二进制文件提交到 `crates/mmkv-core/tests/fixtures/`，Rust 测试无需 C++ 工具链即可跑。
- 提供 `cargo xtask regen-fixtures`（或 `tools/regen.sh`）记录生成参数。
- 每个 fixture 附 `.json` 期望值（key/type/value/actualSize/sequence/version）。

### 7.3 测试层级

| 层级 | 内容 |
|---|---|
| 单元 | varint 边界、CRC 向量、AES-CFB 向量（NIST/OpenSSL）、meta 布局、路径 md5、holder 长度计算 |
| 差分 | Rust↔C++ 双向读写；`dump` 结果逐值比较 |
| 属性 | 随机操作序列（put/remove/clear/trim/rekey）后 C++ 读回一致 |
| 多进程 | 两个子进程交替追加/全量写回；检查内容变更通知与 CRC |
| 故障注入 | 截断文件、篡改 CRC、meta version≥7、零字节文件、只读文件 |
| 模糊 | `cargo-fuzz` 对 payload 解码器；不得 panic/OOM，只返回错误 |
| Miri/ASAN | 核心 unsafe（mmap/AES）在 Miri 不可覆盖的部分用 sanitizer 跑 |

### 7.4 字节级对比

- 对“全量写回”产物，允许 4 字节占位 varint 不同；比较时屏蔽 offset 4..8。
- meta 的 IV/sequence 天然不同；比较其余字段。
- 追加写产物应可做到逐字节一致（除 meta IV 外），作为额外强校验。

---

## 8. 风险清单

| 风险 | 等级 | 对策 |
|---|---|---|
| 加密 offset holder 的 CFB 状态/offset 计算错误 | 高 | 顺序解码不预读；fixtures 覆盖 257/258/1KB/1MB 值；与 C++ 逐字节比对 |
| 多进程锁升级/降级死锁 | 高 | 严格锁序；递归计数；双进程压力测试；超时/中断测试 |
| Rust mmap 重映射导致别名违规 | 中 | 不暴露跨 remap 引用；`State` mutex 串行化；零拷贝闭包持锁 |
| 全量写回字节不完全一致 | 中 | 语义兼容为准；C++ 读回验证；占位 varint 屏蔽比较 |
| 页大小差异（4K/16K） | 中 | 运行时 `page_size`；不硬编码；跨设备文件测试 |
| Apple 目录/保护/扩展语义偏差 | 中 | 用 C++ 打印路径对比；模拟器+真机；扩展 bundle 检测 |
| Android ashmem 复杂度 | 高 | 后置到 P5；首期仅普通文件 |
| NSCoding/NSDate 对象 API 缺失 | 低 | 文档声明；`mmkv-apple` 桥接后续 |
| 依赖 crate（aes/crc32fast/memmap2）行为差异 | 低 | 向量测试；必要时自实现 CFB/CRC |
| 许可/版权遗漏 | 低 | LICENSE/NOTICE；文件头声明 |

---

## 9. 工作量与里程碑

| 阶段 | 人日 | 累计 |
|---|---|---|
| P0 契约与骨架 | 2 | 2 |
| P1 只读明文 | 4–6 | 6–8 |
| P2 明文写入 | 6–9 | 12–17 |
| P3 加密 | 5–7 | 17–24 |
| P4 高级功能/恢复 | 6–9 | 23–33 |
| P5 平台矩阵/多进程 | 6–10 | 29–43 |
| P6 FFI/生态 | 6–10 | 35–53 |

建议里程碑：
- **M1（P1 完成）**：Rust 能读任何明文 MMKV 文件。
- **M2（P2 完成）**：明文功能 1:1，可与 C++ 互写。
- **M3（P3 完成）**：加密 1:1。
- **M4（P4 完成）**：全功能单进程。
- **M5（P5 完成）**：macOS/iOS/Linux 可用，多进程稳定。
- **M6（P6 完成）**：Swift/Kotlin 可用。

---

## 10. 附录

### 10.1 C++ → Rust 模块映射

| C++ 文件 | Rust |
|---|---|
| `Core/MMKV.h` / `MMKV.cpp` | `mmkv-core/store/*`, `mmkv-core/registry.rs` |
| `Core/MMKV_IO.cpp` | `store/load.rs`, `write.rs`, `compact.rs`, `expire.rs`, `backup.rs` |
| `Core/MiniPBCoder.cpp` | `codec/mini_pb.rs` |
| `Core/CodedInputData.cpp` | `codec/input.rs` |
| `Core/CodedOutputData.cpp` | `codec/output.rs` |
| `Core/PBUtility.cpp` | `codec/varint.rs` |
| `Core/MMBuffer.cpp` | `buf.rs` |
| `Core/KeyValueHolder.cpp` | `kv/holder.rs` |
| `Core/MMKVMetaInfo.hpp` | `meta.rs` |
| `Core/MemoryFile*.cpp` | `mmkv-platform-posix` + `memory.rs` |
| `Core/ThreadLock.*` | `parking_lot::Mutex` |
| `Core/InterProcessLock.*` | `lock.rs` + `MappedRegion::lock` |
| `Core/aes/*` | `crypt/aes.rs`, `crypt/cfb.rs` |
| `Core/CodedInputDataCrypt.cpp` | `crypt/input.rs` |
| `Core/crc32/*` | `crc32.rs` |
| `Core/MMKV_OSX.cpp` / `MemoryFile_OSX.cpp` | `mmkv-platform-macos`, `mmkv-platform-ios` |
| `Core/MMKV_Android.cpp` / `MemoryFile_Android.cpp` | `mmkv-platform-android` |
| `Core/cbridge/MMKVBridge.*` | `mmkv-ffi` |
| `iOS/MMKV/libMMKV.mm` | `mmkv`（Rust API）+ 可选 `mmkv-apple`（ObjC 对象桥） |

### 10.2 字节级不变量 checklist

实现完成后逐条打勾：

- [ ] 数据文件 offset 0 是 `u32 LE`，CRC 从 offset 4 开始，长度 = `actualSize`。
- [ ] payload 首字段是伪 map-size varint（通常 4 字节），解码丢弃。
- [ ] 条目 = `varint(keyLen)+key+varint(valueLen)+value`。
- [ ] data-holder 的 `valueLen` 包含内部 `varint(rawLen)`；payload 是 `varint(rawLen)+raw`。
- [ ] 删除 = `valueLen == 0` 的记录；重复 key 后者胜出。
- [ ] `computedKVSize = keySize + varint_size(keySize) + varint_size(valueSize)`。
- [ ] key 长度 ≤ 65531。
- [ ] meta 112 字节、字段偏移/大小端完全一致。
- [ ] version ≥ 4 才读写 `m_flags`；flags bit0 = 过期。
- [ ] CRC-32 为 zlib 语义，链式 `crc = crc32(crc, chunk)`。
- [ ] 加密：AES-CFB128、无 padding、整段 payload 一条流、`num = offset % 16`。
- [ ] 密钥 = 原始字节截断/零填充到 16/32；无 KDF。
- [ ] IV：version ≥ 2 用 meta `m_vector`；否则 `key[0..16]`。
- [ ] 值 > 256 字节才用 offset holder；≤ 256 缓存；≤ 27 内联。
- [ ] offset holder 的 `pbKeyValueSize = varint_size(keySize) + varint_size(valueSize)`。
- [ ] 全量写回：新随机 IV、4 字节占位、按 offset 升序密集重排、`m_sequence++`、`lastConfirmed` 更新。
- [ ] 过期：value 尾部 `u32 LE` Unix 秒；0 = never；读取过期即删。
- [ ] 文件始终页对齐；扩容显式零填充；`msync(MS_SYNC/MS_ASYNC)`。
- [ ] 多进程锁在 `.crc` fd 上；不创建额外锁文件。
- [ ] 特殊字符 id → `specialCharacter/<md5(id)>`；默认 id `mmkv.default`。
- [ ] 实例注册键 = `md5(root + "/" + id)`。
- [ ] 新文件 meta version 最终为 4；旧文件可降级/升级读取。

### 10.3 依赖清单（建议）

| 用途 | crate |
|---|---|
| mmap | `memmap2` |
| POSIX 调用 | `libc` |
| 锁/状态 | `parking_lot` |
| CRC-32 | `crc32fast`（需验证 zlib 语义） |
| AES 块加密 | `aes`（RustCrypto；AArch64/AES-NI 后端） |
| 随机 IV | `getrandom`（Apple 走 `getentropy`） |
| 文件名 MD5 | `md-5`（仅非安全用途） |
| 密钥清零 | `zeroize`（可选） |
| Apple 平台 | `objc2`, `objc2-foundation`, `objc2-ui-kit`（iOS 生命周期） |
| Android（后续） | `jni`, `ndk-context` |
| Windows（后续） | `windows-sys` |
| 测试 | `tempfile`, `proptest`, `criterion` |

不引入：任何 KDF/密码学哈希（MMKV 不用）、serde（格式不是自描述）、数据库引擎。

### 10.4 待确认决策

1. **平台优先级**：默认按 macOS → iOS → Linux → Android → Windows；如果 iOS 优先，可把 P5 的 iOS 部分提前到 P2 之后。
2. **API 返回风格**：本方案默认 `put -> Result`、`get -> Option`；若希望严格 1:1 `bool`，可以保留 `put_raw -> bool` 兼容层。
3. **平台绑定**：已确定使用 objc2，不提供 Swift shim。
4. **默认根目录**：严格沿用 `Documents/mmkv`；应用若希望 `Application Support`，通过 `StoreConfig.root` 显式传入（不改默认，避免破坏 1:1）。
5. **ashmem / ObjC NSCoding**：是否纳入首期。本方案默认后置。
