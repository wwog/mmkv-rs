# 代码 Review：方法一致性、逻辑一致性、Rust 适配性

> 范围：`mmkv-core`、`mmkv-rs` 门面、六个平台 crate
> 基线：C++ MMKV v2.4.3
> 结果：第一轮修复 17 个逻辑/一致性问题；第二轮（let it crash）修复 6 个缺陷；
> 终审修复 3 个失败路径状态不一致。当前 **71 个测试通过**，
> `clippy --workspace --all-targets -D warnings` 干净，四个交叉 target 编译通过。

## 终审（2026-11-09）

覆盖范围：`mmkv-core`（store/加载/写回/加密/过期/多进程/lock/registry/meta/
codec/admin/namespace）、`mmkv-rs` 门面、六个平台 crate、测试与 CI。
逐项对照 C++ v2.4.3 的关键路径：`loadFromFile`/`checkDataValid`、
`writeActualSize`、`reKey`/`checkReSetCryptKey`、`enable/disableAutoKeyExpire`、
`importFrom`、`clearAll`/`trim`、Android 进程模式与 legacy 迁移、Windows
`truncate` 顺序、Apple 根目录/生命周期。

### 终审修复：失败路径不能留下“内存认为已生效、文件没生效”的状态

| # | 问题 | 影响 | 修复 |
|---|---|---|---|
| F1 | `rekey` 在重写前就替换 `inner.crypter` | 重写失败（容量/随机数错误）后，后续读写会拿新 key 解旧文件，等于整库不可读 | 保留旧 crypter，写成功后才提交；失败回滚 |
| F2 | `enable_expiration` 迁移失败后内存标志已打开 | 后续 `put` 写入带时间戳，但 meta 没有 expiration flag；重载后向量类值会把时间戳当成元素解析 | 失败时回滚 expiration/compare/flags/version |
| F3 | `disable_expiration` 迁移失败后内存标志已关闭 | 后续 `put` 不带时间戳，但文件 meta 仍标记 expiration；重载后新值会被裁掉尾部 4 字节 | 同样回滚 |

这三处都只在“写 payload 之前”失败时回滚（容量检查、随机数、编码检查）；
若真的写了一半再失败，文件已经不一致，按 let it crash 直接返回 `Err`，
不做半吊子恢复。

### 终审判定

- 核心格式、CRC/meta、加密、过期、多进程刷新、备份恢复与 C++ 语义一致；
  C++ 生成的明文/加密/过期 fixtures 全部可读。
- 调用方误用（handler 重入、错误 key/模式）按 let it crash 处理：返回错误或
  panic，不再用防御性逻辑掩盖。
- 剩余风险集中在**环境**而非逻辑：非 macOS 平台未真机运行、Android ashmem
  跨进程 FD 交接未实现、非 UTF-8 key/value、Apple SIGBUS 级磁盘错误检测。
  这些都已记录在 [GAPS.md](GAPS.md)，不阻塞当前 Rust 应用场景。
- 未为失败路径回滚新增测试：需要注入 IO/随机数故障才能稳定复现；代码路径
  已通过快照+恢复保持简单可审计。

## 0. 第二轮 Review（2026-11-09）：让错误暴露，而不是层层兜底

> 原则：只有“数据/环境错误”（CRC、IO、损坏文件）需要恢复策略；
> “调用方写错代码”（handler 重入、错误模式、错误配置）不在库内兜底，
> 直接返回错误或 panic，对应 Erlang 的 let it crash。

### 0.1 本轮修复

| # | 问题 | 影响 | 修复 |
|---|---|---|---|
| 0.1 | last-confirmed 恢复后 CRC 种子未修复 | 恢复后继续追加，下次加载 CRC 失败，追加的数据丢失 | `validate` 同时返回恢复后的 CRC；`load_locked` 按 C++ `checkDataValid` 的顺序写回 meta |
| 0.2 | `reset_crypt_key` 只换 crypter、不重载 | 外部 rekey 后内存里仍是旧 key 解出的字典，读写会看到空库 | 与 C++ `checkReSetCryptKey` 一致：清缓存 → 换 key → reload |
| 0.3 | handler 回调内重入同一 store 自死锁 | 现场是 hang，没有信息 | handler 期间设置线程标记；重入时 panic 并给出明确信息 |
| 0.4 | `init` 立即 drop 平台事件取消句柄 | iOS 的内存警告/前后台观察者被立即移除，生命周期跟踪失效 | 观察者保留到进程结束（与 C++ 相同） |
| 0.5 | registry key 用未规范化 root，store 用规范化 root | 符号链接/后创建目录下 `forget`/`close_instance` 找不到实例 | `open_registered` 先规范化 root，key 与 `store.root` 一致 |
| 0.6 | Windows `resize` 在映射存活时 `SetEndOfFile` | Windows 上 trim/clear 可能因 `ERROR_USER_MAPPED_FILE` 失败 | 先 unmap，再 set_len/zero-fill，失败时恢复旧映射（与 C++ 顺序一致） |

另外：`put_*` 现在会刷新 `last_access`，自动清理不会把持续写入的实例当成空闲。

新增回归测试：

- `last_confirmed_recovery_keeps_following_appends`：篡改 meta CRC、保留
  last-confirmed 快照，恢复后追加并重开，验证不丢数据。
- `reset_crypt_key_handles_plain_and_cipher_directions`：明文 → 密文 → 明文，
  `reset_crypt_key` 自身完成重载，不再依赖调用方 `clear_memory_cache`。
- `handler_reentry_panics_instead_of_deadlocking`：回调重入直接 panic。

### 0.2 明确定位为“不修”

- **getter 吞错**（原 §2.2）：`get_*` 返回 `Option`/默认值，与 C++ 的
  `hasValue=false` 对齐；真正的错误路径是写操作，已经是 `Result`。不新增
  整套 `try_*`。
- **void mutator**（原 §2.3）：`clear`/`trim`/`sync` 保持 C++ 的 void 语义，
  不为每个 IO 错误增加返回通道。
- **handler 时序重构**（原 §2.1）：不把通知挪到锁外。锁内通知是 C++ 的既有
  语义，挪动会引入“通知时状态已变”的新竞态；改为 panic 更诚实。
- **`open_at` 忽略 `config.root`**（原 §2.6）：保留“显式 root 优先”，在文档
  中写明，不用 debug assert 惩罚 `Store::open` 的内部调用。

### 0.3 仍然保留的已知差异

见 [GAPS.md](GAPS.md)。本轮没有改变格式/字节层面的兼容性：C++ 生成的
明文/加密 fixtures 仍可读，Rust 写出的文件仍可被 C++ 读取。

## 1. 已修复的问题

### 1.1 多进程丢写（高）

写入路径原先先刷新 meta、再取独占进程锁，两个进程可在刷新与写入之间交错，
后写者用旧的 `actual_size` 覆盖前写者。现在 `put_blob` **先取独占锁，再
`ensure_loaded_locked`**，与 C++ 的 `m_exclusiveProcessLock -> checkLoadData`
顺序一致。由新增的真实双进程压力测试发现并验证。

### 1.2 加载恢复路径被跳过（高）

`load_locked` 在 `need_load == true` 时调用 `full_writeback_locked`，而后者
会因 `need_load` 直接返回，导致 CRC 恢复写回和 legacy IV 升级被静默跳过。
同时 `has_full_writeback` 可能残留 true。现在：

- 加载开始重置 `has_full_writeback = false`；
- 进入写回前设置 `need_load = false`；
- `full_writeback_locked` 自身获取独占进程锁；
- 空文件/丢弃分支的 meta 写入也持独占锁。

### 1.3 全量重写先清空字典（高）

`write_all_entries_locked` 原先先 `dict.clear()` 再逐条写；中途失败会丢失
内存字典。现在写入本地 `new_dict/new_dict_crypt`，全部成功后才替换。

### 1.4 只读实例的文件增长/缺失处理（中高）

`load_locked` 在只读且 `data == None` 时直接返回，既没有重新 mmap，也可能
保留旧字典。现在总是尝试重新打开只读映射；失败时清空字典并作为空实例返回
（贴近 C++ 行为）。

### 1.5 Windows 只读映射越界（中高）

只读打开时按页向上取整映射长度，可能超过实际文件大小。现在只读映射使用
真实文件大小；`resize` 也改为先 `set_len`/零填充、再 unmap/remap，失败时
保留旧映射。

### 1.6 `close()` 不释放锁（中高）

`close(&self)` 只置 `closed`，文件锁和 Android 进程模式锁要等所有 `Arc`
释放才释放，会阻塞其他进程。现在：

- `FileLock::release_all()` 清零计数、解锁并禁用后续锁定；
- 进程模式 guard 改为 `Mutex<Option<Box<dyn Send+Sync>>>`，`close` 时取走释放；
- `lock()/try_lock()` 对已关闭实例返回 `Closed`/`None`。

### 1.7 注册表构造期持锁（中高，Rust 适配）

`registry::open` 原先在持有实例表 `Mutex` 时执行 `Store::open_at`；若
handler 回调再次调用 `open`，会自死锁。现在使用 `ReentrantMutex<()>` 串行化
构造，构造后再次查表；同线程可重入，败者 store 被关闭。避免重复写实例。

### 1.8 只读配置/锁语义（中）

- 只读实例以 `config.expiration = Some(...)` 打开会错误返回 `ReadOnly`；
  现在只读时仅记录 `expired_in_seconds` 并成功打开。
- 只读写入路径（put/remove/clear/trim/rekey）在取进程锁前返回 `ReadOnly`，
  避免在 Windows 只读句柄上请求独占锁。
- `sync` 对只读实例使用共享锁。
- `import_from` 的只读源使用共享锁。

### 1.9 备份/恢复完整性（中）

`backup/restore` 原先允许源 `.crc` 缺失，可能写出“数据新、meta 空”的损坏
目标。现在源数据与 `.crc` 都必须存在；恢复仍持目标 `.crc` 独占锁。

### 1.10 Android 只读副作用（中）

只读打开不再执行 legacy 迁移，也不再执行进程模式检查（后者需要写打开
`.crc`）。

### 1.11 其它修复

- `recover_unreadable` 隔离文件名改为追加 `.corrupt.<timestamp>`，不再
  `with_extension` 丢掉原扩展名。
- 加密全量写回/`rekey`/过期迁移遇到无法解密的 holder 现在返回
  `Corrupted`，不再静默丢 key。
- 自动清理的 `last_access` 在建实例时即更新，否则“开过但未访问”的实例
  永远不会被回收。
- 删除 `.crc` 锁文件以读写方式打开，兼容 Windows `LockFileEx` 独占锁。
- 补全方法一致性：`put_*_vec_expiring`、`put_strings_expiring`、
  `get_bytes_or`、`get_strings_or`、`get_str_into`、`with_bytes`、`with_str`、
  `enumerate_keys`、`enumerate_keys_non_expired`。
- Android `Mode::CONTEXT_MODE_MULTI_PROCESS` / `BACKUP` 位、iOS 初始后台状态。

## 2. 仍然存在的问题（按优先级）

### 2.1 Handler 回调在内部锁内执行（高，Rust 适配）

`Handler::log`、`on_crc_check_fail`、`on_content_loaded`、
`on_content_changed` 可能在持有 `inner: parking_lot::Mutex<Inner>` 时被调用。
若 handler 在同一线程回调同一实例的 `get/put`，会**自死锁**（
`parking_lot::Mutex` 不可重入；C++ 使用递归 `pthread_mutex`）。

建议二选一：

1. 把 `on_content_loaded/changed` 改为加载完成后在锁外触发（CRC handler
   可先记录再锁外询问），日志回调只保证不在状态锁内调用；
2. 或提供显式的“回调内不可重入”文档，并把 handler 设计为纯通知（不访问
   store）。

当前未改，因为需要重构通知时序；这是本版最重要的 Rust 适配风险。

### 2.2 Getter 隐藏错误（中）

`get_*`/`count`/`all_keys` 把 `Closed`、IO、解码错误折叠为 `None`/0/空集合。
Rust 项目通常希望区分“不存在”和“读取失败”。建议增加：

```rust
pub fn try_get_i32(&self, key: &str) -> Result<Option<i32>>;
pub fn try_all_keys(&self) -> Result<Vec<String>>;
```

保留现有便捷 API。

### 2.3 void 写操作吞错（中）

`clear`、`clear_keep_space`、`trim`、`sync`、`sync_async`、
`clear_memory_cache`、`check_content_changed` 返回 `()` 并忽略错误。
建议返回 `Result<()>`，或增加 `try_*` 变体。C++ 是 void，但 Rust 的 IO
错误不应静默。

### 2.4 `close(&self)` 语义（中）

Rust 无法让已有 `Arc<Store>` 悬空，因此 `close` 只是标记 closed；其他克隆
继续存在并返回 `Error::Closed`。这与 C++ “delete 后不可用”不同但更安全。
建议文档明确，或提供消费型 `Mmkv::close(self)` 包装。

### 2.5 核心静态 API 不一致（中）

`Store::remove_storage/backup_one/restore_one/backup_all/restore_all` 需要
`&dyn Platform`（为了加锁/复制），而 `is_file_valid/exists` 不需要。
建议：

- 提供 `StoreAdmin<'a>`（绑定 platform）承载这些操作；
- 或给不需要锁的版本明确命名 `*_unlocked`。

当前门面已隐藏该差异，直接使用 core 的用户会感到不一致。

### 2.6 `open_at` 忽略 `config.root`（低）

`Store::open` 使用 `config.root`，`Store::open_at` 使用传入 root 并忽略
`config.root`。这是刻意给 `Namespace` 用，但容易误用。建议 debug assert
`config.root.is_none()`，或在文档中明确“open_at 的 root 优先”。

### 2.7 `with_bytes/with_str` 不是零拷贝（低）

闭包执行期间虽然持锁，但 `raw_blob` 已把值复制到 `Vec`。若需要真正零拷贝，
应提供在锁内直接对 mmap 切片回调的 API（注意 remap 失效，需要同一锁域）。
当前实现是“作用域内读取”，不是“零分配读取”。

### 2.8 非 UTF-8 key/value（低）

内部 key 是 `Box<[u8]>`，公开 API 只接受 `&str`；`all_keys` 对非法 UTF-8
lossy。C++ `std::string` 支持任意字节。若需要完全兼容，建议引入
`Key(&[u8])` 或 `put_bytes_key` 系列。

### 2.9 只读缺失 `.crc`（低）

C++ 返回空实例；当前 `open_at` 返回 `FileNotValid`。可增加匿名空 meta
（`Vec`/匿名 mmap 实现的 `MappedRegion`）来对齐。

### 2.10 其它

- Android ashmem 跨进程 FD 交接/管理 API（`ashmemFD`/`mmapWithAshmemFD`）。
- `AesCrypt` 内部 key material 未 zeroize（`CryptKey` 已清零）。
- 自动清理线程没有停止/join 机制；进程退出自然结束。
- `enable_expiration`/`disable_expiration` 若全量写回失败，内存
  `expiration_enabled` 可能已变更；建议失败回滚。
- 备份/恢复不是原子 rename；Linux 恢复用 `sendfile`，备份用 `std::fs::copy`。
- Apple mmap 磁盘错误检测是 `pread` 近似，非 SIGBUS 级。
- 32 位、Windows/iOS/Android/Linux 运行期未验证。

## 3. 方法一致性对照

| C++ | Rust | 说明 |
|---|---|---|
| `set(v,key)` / `set(v,key,expire)` | `put_*` / `put_*_expiring` | 覆盖标量、字符串、bytes、数组 |
| `getX(key,default,&hasValue)` | `get_x(key, default)` / `get_x_opt(key)` | `hasValue` → `Option` |
| `getString(key,out)` | `get_str` / `get_str_or` / `get_str_into` / `with_str` | 补全 |
| `getBytes` | `get_bytes` / `get_bytes_or` / `with_bytes` | 补全 |
| `getVector` | `get_strings` / `get_*_vec` | 补全 |
| `set(vector,key,expire)` | `put_*_vec_expiring` | 本轮补全 |
| `allKeys(filter)` | `all_keys` / `all_keys_non_expired` | 拆分 |
| `count(filter)` | `count` / `count_non_expired` | 拆分 |
| `clearAll(keepSpace)` | `clear` / `clear_keep_space` | 拆分 |
| `sync(SyncFlag)` | `sync` / `sync_async` | 拆分 |
| `lock/unlock/try_lock` | `lock() -> Guard` / `try_lock() -> Option<Guard>` | RAII，无 `unlock` |
| `reKey` / `checkReSetCryptKey` | `rekey` / `reset_crypt_key` | 返回 `Result` |
| `enableAutoKeyExpire` | `enable_expiration` | 返回 `Result` |
| `removeStorage` | `Store::remove_storage(..., platform)` / 门面 `remove_storage` | core 多一个 platform 参数 |
| `backupOne/restoreOne` | 同上 | 加锁/复制需要 platform |
| `isFileValid/checkExist` | `is_file_valid/exists` | 无 platform |
| `registerHandler` | `set_handler` | 全局 |
| `onExit` | `shutdown` | |
| `lock_thread/unlock_thread` | 未暴露 | 内部 Mutex |
| `NSObject/NSCoding` | 未提供 | ObjC 对象 API，Rust 范围外 |
| `enableAutoCleanUp` | `enable_auto_cleanup` | 后台状态跳过 |
| `pageSize/version/getRootDir` | `page_size` / `VERSION` / `root_dir` | 门面 |

## 4. Rust 适配性结论

**做得好的部分**

- 句柄是 `Arc<Store>`，内部状态 `parking_lot::Mutex`；`Store` 为 `Send + Sync`。
- 公开 API 无裸指针；`Result`/`Option` 明确；builder 式 `StoreConfig`。
- `Platform` 为 dyn-safe 契约，平台 crate 编译期选择；无 C++/Swift 构建依赖。
- `CryptKey` 的 `Debug` 脱敏、Drop 清零；错误类型 `Error` 实现 `Display`/`Error`。
- 文件锁 RAII guard、`close` 释放锁、真双进程测试。

**需要继续改进**

1. Handler 重入（§2.1）——最重要的 Rust 适配缺口。
2. 错误语义（§2.2/2.3）——Getter/void mutator 静默吞错。
3. 核心静态 API 的 platform 参数（§2.5）。
4. 零拷贝读取与非 UTF-8 key（§2.7/2.8）。
5. `close(&self)` 语义文档（§2.4）。
6. 运行期平台验证（见 [GAPS.md](GAPS.md)）。
