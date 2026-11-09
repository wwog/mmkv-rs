//! The MMKV store engine.
//!
//! This is the Rust counterpart of `MMKV.cpp` + `MMKV_IO.cpp`: the append-only
//! log, in-memory holders, full write-back, metadata updates, multi-process
//! refresh, optional AES-CFB128 encryption and the public get/set surface.

mod admin;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use mmkv_platform::{FileLockKind, Platform, SyncMode, initial_capacity};
use parking_lot::Mutex;

use crate::codec::varint::{varint_size_i32, varint_size_u32};
use crate::codec::{CodedInput, CodedOutput, mini_pb};
use crate::config::{CryptKey, Expiration, Mode, StoreConfig};
use crate::crc32::crc32;
use crate::crypt::{AES_IV_LEN, AesCrypt, CryptReader, KeyValueHolderCrypt};
use crate::error::{CorruptKind, Error, Recover, Result};
use crate::kv::{KEY_SIZE_LIMIT, KeyValueHolder};
use crate::lock::{FileLock, ProcessGuard};
use crate::log::{handler, mmkv_log};
use crate::memory::MemoryFile;
use crate::meta::{self, MetaInfo};
use crate::path::{
    SPECIAL_CHARACTER_DIRECTORY_NAME, absolute_path, crc_path, has_special_characters,
    mapped_kv_path,
};

/// MMKV writes a fake dictionary length first; normally four bytes.
const ITEM_SIZE_HOLDER_SIZE: usize = 4;
const FIXED32_SIZE: usize = 4;
/// A 4-byte varint placeholder value (`0x200000` is encoded as 4 bytes).
const FAKE_MAP_SIZE: u32 = 0x20_0000;

pub type Mmkv = Arc<Store>;
type Dictionary = HashMap<Box<[u8]>, KeyValueHolder>;
type CryptDictionary = HashMap<Box<[u8]>, KeyValueHolderCrypt>;
type Entry = (Box<[u8]>, Option<KeyValueHolder>);
type CryptEntry = (Box<[u8]>, Option<KeyValueHolderCrypt>);
type RawEntries = Vec<(Box<[u8]>, Vec<u8>)>;

struct Inner {
    data: Option<MemoryFile>,
    meta: MemoryFile,
    expected_capacity: usize,
    meta_info: MetaInfo,
    dict: Dictionary,
    dict_crypt: CryptDictionary,
    crypter: Option<AesCrypt>,
    actual_size: usize,
    output_position: usize,
    need_load: bool,
    has_full_writeback: bool,
    crc_digest: u32,
    closed: bool,
    expiration_enabled: bool,
    expired_in_seconds: u32,
    compare_before_set: bool,
    item_size_limit: u32,
    recover: Option<Recover>,
    read_only: bool,
    multi_process: bool,
}

pub struct Store {
    id: String,
    root: PathBuf,
    path: PathBuf,
    crc_path: PathBuf,
    mode: Mode,
    platform: Arc<dyn Platform>,
    lock: FileLock,
    process_mode_guard: Mutex<Option<Box<dyn Send + Sync>>>,
    last_access: AtomicU64,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("id", &self.id)
            .field("path", &self.path)
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl Store {
    pub fn open(
        platform: Arc<dyn Platform>,
        id: impl Into<String>,
        config: &StoreConfig<'_>,
    ) -> Result<Mmkv> {
        let root = match config.root {
            Some(root) => absolute_path(root),
            None => platform.default_root_dir()?,
        };
        Self::open_at(platform, &root, id, config)
    }

    pub fn open_at(
        platform: Arc<dyn Platform>,
        root: &Path,
        id: impl Into<String>,
        config: &StoreConfig<'_>,
    ) -> Result<Mmkv> {
        let id = id.into();
        if id.is_empty() {
            return Err(Error::InvalidId(id));
        }
        let root = absolute_path(root);
        let read_only = config.mode.is_read_only();
        let multi_process = config.mode.is_multi_process();

        if config.mode.is_ashmem() {
            if read_only {
                return Err(Error::Unsupported("read-only ashmem stores"));
            }
            let page_size = platform.page_size();
            let size = initial_capacity(config.expected_capacity.max(page_size), page_size);
            let root = PathBuf::from("/dev/ashmem");
            let path = root.join(&id);
            let crc_path = crate::path::crc_path(&path);
            let meta_region = platform.open_ashmem(&format!("{id}.crc"), page_size)?;
            let meta = MemoryFile::from_region(meta_region, false, page_size, crc_path.clone());
            let data_region = platform.open_ashmem(&id, size)?;
            let data = MemoryFile::from_region(data_region, false, page_size, path.clone());
            let lock = match meta.try_clone_file() {
                Some(Ok(file)) => {
                    FileLock::new_with_kind(platform.clone(), file, FileLockKind::Record)
                }
                _ => FileLock::disabled(),
            };
            let crypter = match config.crypt_key {
                Some(key) if !key.is_empty() => Some(AesCrypt::new(key.as_bytes(), config.aes256)),
                _ => None,
            };
            let inner = Inner {
                data: Some(data),
                meta,
                expected_capacity: size,
                meta_info: MetaInfo::default(),
                dict: Dictionary::new(),
                dict_crypt: CryptDictionary::new(),
                crypter,
                actual_size: 0,
                output_position: 0,
                need_load: true,
                has_full_writeback: false,
                crc_digest: 0,
                closed: false,
                expiration_enabled: false,
                expired_in_seconds: 0,
                compare_before_set: config.compare_before_set && config.crypt_key.is_none(),
                item_size_limit: config.item_size_limit,
                recover: config.recover,
                read_only: false,
                multi_process: true,
            };
            let store = Arc::new(Store {
                id,
                root,
                path,
                crc_path,
                mode: config.mode,
                platform,
                lock,
                process_mode_guard: Mutex::new(None),
                last_access: AtomicU64::new(0),
                inner: Mutex::new(inner),
            });
            store.touch();
            {
                let mut inner = store.inner.lock();
                if let Some(expiration) = config.expiration {
                    let _guard = store.lock.exclusive_guard()?;
                    store.load_locked(&mut inner)?;
                    store.configure_expiration_locked(&mut inner, expiration)?;
                } else {
                    let _guard = store.lock.shared_guard()?;
                    store.load_locked(&mut inner)?;
                }
            }
            return Ok(store);
        }

        if !read_only {
            platform.migrate_legacy_store(&root, &id)?;
        }

        if !read_only {
            platform.create_dir_all(&root)?;
            if has_special_characters(&id) {
                platform.create_dir_all(&root.join(SPECIAL_CHARACTER_DIRECTORY_NAME))?;
            }
        }

        let path = mapped_kv_path(&root, &id);
        let crc_path = crc_path(&path);
        let page_size = platform.page_size();
        let expected_capacity = initial_capacity(config.expected_capacity, page_size);

        if !read_only {
            // Approximate MMKV's Apple `checkFileHasDiskError`: an unreadable
            // mapping source is moved aside so the store can rebuild.
            recover_unreadable(&crc_path);
            recover_unreadable(&path);
        }

        let meta = MemoryFile::open(platform.as_ref(), &crc_path, 0, read_only, !multi_process)
            .map_err(|_| Error::FileNotValid {
                path: crc_path.to_string_lossy().into_owned(),
            })?;

        let lock = if multi_process {
            match meta.try_clone_file() {
                Some(Ok(file)) => FileLock::new(platform.clone(), file),
                _ => FileLock::disabled(),
            }
        } else {
            FileLock::disabled()
        };

        let (process_mode_ok, process_mode_guard) = if read_only {
            (true, None)
        } else {
            platform.check_process_mode(&crc_path, multi_process)?
        };
        if !process_mode_ok {
            return Err(Error::ProcessModeMismatch);
        }

        let data =
            match MemoryFile::open(platform.as_ref(), &path, expected_capacity, read_only, true) {
                Ok(file) => Some(file),
                Err(err) => {
                    if read_only {
                        None
                    } else {
                        return Err(err.into());
                    }
                }
            };

        let crypter = match config.crypt_key {
            Some(key) if !key.is_empty() => Some(AesCrypt::new(key.as_bytes(), config.aes256)),
            _ => None,
        };

        let inner = Inner {
            data,
            meta,
            expected_capacity,
            meta_info: MetaInfo::default(),
            dict: Dictionary::new(),
            dict_crypt: CryptDictionary::new(),
            crypter,
            actual_size: 0,
            output_position: 0,
            need_load: true,
            has_full_writeback: false,
            crc_digest: 0,
            closed: false,
            expiration_enabled: false,
            expired_in_seconds: 0,
            compare_before_set: config.compare_before_set && config.crypt_key.is_none(),
            item_size_limit: config.item_size_limit,
            recover: config.recover,
            read_only,
            multi_process,
        };

        let store = Arc::new(Store {
            id,
            root,
            path,
            crc_path,
            mode: config.mode,
            platform,
            lock,
            process_mode_guard: Mutex::new(process_mode_guard),
            last_access: AtomicU64::new(0),
            inner: Mutex::new(inner),
        });
        store.touch();

        {
            let mut inner = store.inner.lock();
            if let Some(expiration) = config.expiration {
                let _guard = store.lock.exclusive_guard()?;
                store.load_locked(&mut inner)?;
                store.configure_expiration_locked(&mut inner, expiration)?;
            } else {
                let _guard = store.lock.shared_guard()?;
                store.load_locked(&mut inner)?;
            }
        }

        Ok(store)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn crc_path(&self) -> &Path {
        &self.crc_path
    }

    pub fn is_multi_process(&self) -> bool {
        self.mode.is_multi_process()
    }

    pub fn is_read_only(&self) -> bool {
        self.mode.is_read_only()
    }

    pub fn is_closed(&self) -> bool {
        self.inner.lock().closed
    }

    fn touch(&self) {
        self.last_access
            .store(self.platform.now_unix_seconds() as u64, Ordering::Relaxed);
    }

    pub fn last_access_seconds(&self) -> u64 {
        self.last_access.load(Ordering::Relaxed)
    }

    pub fn is_in_background(&self) -> bool {
        self.platform.app_state() == mmkv_platform::AppState::Background
    }

    pub fn is_expiration_enabled(&self) -> bool {
        self.inner.lock().expiration_enabled
    }

    pub fn is_compare_before_set_enabled(&self) -> bool {
        let inner = self.inner.lock();
        inner.compare_before_set && !inner.expiration_enabled && inner.crypter.is_none()
    }

    pub fn is_encryption_enabled(&self) -> bool {
        self.inner.lock().crypter.is_some()
    }

    /// The configured crypt key bytes, if encryption is enabled.
    pub fn crypt_key(&self) -> Option<Vec<u8>> {
        self.inner
            .lock()
            .crypter
            .as_ref()
            .map(|crypter| crypter.key().to_vec())
    }

    /// Transform plaintext into encrypted text (or vice versa with `None`),
    /// or change to a different key. MMKV rewrites the whole store and
    /// generates a fresh IV when encryption remains enabled.
    pub fn rekey(&self, key: Option<&CryptKey>, aes256: bool) -> Result<()> {
        let mut inner = self.inner.lock();
        if inner.closed {
            return Err(Error::Closed);
        }
        if inner.read_only {
            return Err(Error::ReadOnly);
        }
        let _guard = self.lock.exclusive_guard()?;
        self.ensure_loaded_locked(&mut inner)?;

        match (inner.crypter.as_ref(), key) {
            (Some(crypter), Some(key)) if crypter.matches_key(key.as_bytes(), aes256) => {
                return Ok(());
            }
            (None, None) => return Ok(()),
            (None, Some(key)) if key.is_empty() => return Ok(()),
            _ => {}
        }

        if inner.expiration_enabled {
            self.filter_expired_locked(&mut inner);
        }
        let entries = self.all_values_raw_locked(&mut inner)?;
        let new_crypter = key
            .filter(|key| !key.is_empty())
            .map(|key| AesCrypt::new(key.as_bytes(), aes256));
        inner.crypter = new_crypter;
        if inner.crypter.is_some() {
            inner.compare_before_set = false;
        }
        self.write_all_entries_locked(&mut inner, entries, true)
    }

    /// Update the in-memory crypt key without rewriting the file. This mirrors
    /// `checkReSetCryptKey`, used after another process called `rekey`.
    pub fn reset_crypt_key(&self, key: Option<&CryptKey>, aes256: bool) -> Result<()> {
        let mut inner = self.inner.lock();
        if inner.closed {
            return Err(Error::Closed);
        }
        self.ensure_loaded_locked(&mut inner)?;
        inner.crypter = key
            .filter(|key| !key.is_empty())
            .map(|key| AesCrypt::new(key.as_bytes(), aes256));
        let iv = if inner.meta_info.version >= meta::VERSION_RANDOM_IV {
            Some(inner.meta_info.vector)
        } else {
            None
        };
        if let Some(crypter) = inner.crypter.as_mut() {
            crypter.reset_iv(iv.as_ref().map(|iv| iv.as_slice()));
        }
        Ok(())
    }

    /// Mark this handle closed and drop its cached state. Other clones observe
    /// `Error::Closed` afterwards.
    pub fn close(&self) {
        crate::registry::forget(self);
        self.lock.release_all();
        let _ = self.process_mode_guard.lock().take();
        let mut inner = self.inner.lock();
        inner.closed = true;
        inner.dict.clear();
        inner.dict_crypt.clear();
        inner.crypter = None;
        inner.data = None;
        inner.need_load = true;
        inner.actual_size = 0;
        inner.output_position = 0;
    }

    // ── loading ─────────────────────────────────────────────────────────

    fn load_locked(&self, inner: &mut Inner) -> Result<()> {
        // Another process may have grown or shrunk the file; remap before
        // trusting the old mapping (MMKV does the same in loadFromFile).
        if let Some(data) = inner.data.as_ref() {
            let disk_size = data.actual_file_size().unwrap_or_else(|_| data.size());
            if disk_size != data.size() {
                inner.data = None;
            }
        }
        if inner.data.is_none() {
            match MemoryFile::open(
                self.platform.as_ref(),
                &self.path,
                inner.expected_capacity,
                inner.read_only,
                true,
            ) {
                Ok(file) => inner.data = Some(file),
                Err(err) => {
                    if inner.read_only {
                        // Missing/invalid data file in read-only mode: behave
                        // like MMKV's empty instance instead of failing the load.
                        inner.dict.clear();
                        inner.dict_crypt.clear();
                        inner.actual_size = 0;
                        inner.output_position = 0;
                        inner.need_load = false;
                        return Ok(());
                    }
                    return Err(err.into());
                }
            }
        }

        let mut meta_info =
            MetaInfo::read(inner.meta.bytes()).ok_or_else(|| Error::FileNotValid {
                path: self.crc_path.to_string_lossy().into_owned(),
            })?;

        // Specious meta (version >= Holder): reset to the last version whose
        // fields are still trustworthy, exactly like loadMetaInfoAndCheck().
        if !inner.read_only && meta_info.version >= meta::VERSION_HOLDER {
            mmkv_log!(
                crate::handler::Level::Warning,
                "[{}] meta file in specious state, version {}",
                self.id,
                meta_info.version
            );
            meta_info.version = meta::VERSION_ACTUAL_SIZE;
            meta_info.flags = 0;
            meta_info.write(inner.meta.bytes_mut());
        }

        if !inner.read_only && meta_info.version < meta::VERSION_FLAG && meta_info.flags != 0 {
            meta_info.flags = 0;
            meta_info.write(inner.meta.bytes_mut());
        }

        inner.expiration_enabled = meta_info.version >= meta::VERSION_FLAG
            && meta_info.version < meta::VERSION_HOLDER
            && meta_info.has_flag(meta::FLAG_ENABLE_KEY_EXPIRE);

        let file_size = inner
            .data
            .as_ref()
            .map(MemoryFile::size)
            .unwrap_or_default();

        let mut actual_size = if meta_info.version >= meta::VERSION_ACTUAL_SIZE {
            meta_info.actual_size as usize
        } else if file_size >= FIXED32_SIZE {
            let bytes = inner.data.as_ref().expect("data exists").bytes();
            u32::from_le_bytes(bytes[0..4].try_into().expect("length checked")) as usize
        } else {
            0
        };

        let (load, need_full_writeback) = {
            let bytes = inner.data.as_ref().expect("data exists").bytes();
            Self::validate(
                bytes,
                file_size,
                &meta_info,
                &mut actual_size,
                inner.recover,
                &self.id,
            )
        };

        inner.meta_info = meta_info;
        inner.crc_digest = inner.meta_info.crc_digest;
        inner.has_full_writeback = false;

        if let Some(crypter) = inner.crypter.as_mut() {
            let iv = if inner.meta_info.version >= meta::VERSION_RANDOM_IV {
                Some(inner.meta_info.vector)
            } else {
                None
            };
            crypter.reset_iv(iv.as_ref().map(|iv| iv.as_slice()));
        }

        if load && actual_size > 0 {
            if inner.crypter.is_some() {
                let entries = {
                    let bytes = inner
                        .data
                        .as_ref()
                        .expect("data exists")
                        .bytes()
                        .get(FIXED32_SIZE..FIXED32_SIZE + actual_size)
                        .ok_or(Error::Corrupted(CorruptKind::Length))?;
                    let crypter = inner.crypter.as_ref().expect("crypter exists");
                    match decode_entries_crypt(bytes, 0, need_full_writeback, crypter) {
                        Ok(entries) => entries,
                        Err(err) => {
                            mmkv_log!(
                                crate::handler::Level::Error,
                                "[{}] encrypted decode failed: {:?}",
                                self.id,
                                err
                            );
                            Vec::new()
                        }
                    }
                };
                inner.dict.clear();
                inner.dict_crypt.clear();
                apply_crypt_entries(&mut inner.dict_crypt, entries);
                let status = {
                    let bytes = inner
                        .data
                        .as_ref()
                        .expect("data exists")
                        .bytes()
                        .get(FIXED32_SIZE..FIXED32_SIZE + actual_size)
                        .ok_or(Error::Corrupted(CorruptKind::Length))?;
                    inner
                        .crypter
                        .as_ref()
                        .expect("crypter exists")
                        .status_at(bytes, actual_size)
                };
                inner
                    .crypter
                    .as_mut()
                    .expect("crypter exists")
                    .set_status(&status);
                inner.actual_size = actual_size;
                inner.output_position = actual_size;
                inner.need_load = false;
                let upgrade_iv = inner.meta_info.version < meta::VERSION_RANDOM_IV;
                if (need_full_writeback || upgrade_iv) && !inner.read_only {
                    self.full_writeback_locked(inner)?;
                }
            } else {
                let entries = {
                    let bytes = inner
                        .data
                        .as_ref()
                        .expect("data exists")
                        .bytes()
                        .get(FIXED32_SIZE..FIXED32_SIZE + actual_size)
                        .ok_or(Error::Corrupted(CorruptKind::Length))?;
                    match decode_entries(bytes, 0, need_full_writeback) {
                        Ok(entries) => entries,
                        Err(err) => {
                            mmkv_log!(
                                crate::handler::Level::Error,
                                "[{}] decode failed: {:?}",
                                self.id,
                                err
                            );
                            Vec::new()
                        }
                    }
                };
                inner.dict.clear();
                inner.dict_crypt.clear();
                apply_entries(&mut inner.dict, entries);
                inner.actual_size = actual_size;
                inner.output_position = actual_size;
                inner.need_load = false;
                if need_full_writeback && !inner.read_only {
                    self.full_writeback_locked(inner)?;
                }
            }
        } else {
            inner.dict.clear();
            inner.dict_crypt.clear();
            inner.actual_size = 0;
            inner.output_position = 0;
            inner.need_load = false;
            if !inner.read_only {
                let _guard = self.lock.exclusive_guard()?;
                if actual_size > 0 {
                    self.write_actual_size_locked(inner, 0, 0, true, None)?;
                    self.sync_locked(inner, SyncMode::Sync)?;
                } else {
                    self.write_actual_size_locked(inner, 0, 0, false, None)?;
                }
            }
        }

        inner.need_load = false;
        self.notify_content_loaded();
        Ok(())
    }

    /// Mirrors `checkDataValid`: returns `(load, need_full_writeback)` and may
    /// rewrite `actual_size` to the last confirmed or recovered value.
    fn validate(
        data: &[u8],
        file_size: usize,
        meta: &MetaInfo,
        actual_size: &mut usize,
        recover: Option<Recover>,
        id: &str,
    ) -> (bool, bool) {
        if file_size < FIXED32_SIZE {
            mmkv_log!(
                crate::handler::Level::Error,
                "[{}] file size {} is too small",
                id,
                file_size
            );
            *actual_size = 0;
            return (false, false);
        }

        // Downgrade & upgrade: an older build may have written the legacy
        // 4-byte header while meta still carries a valid CRC.
        if meta.version >= meta::VERSION_ACTUAL_SIZE {
            let old_style =
                u32::from_le_bytes(data[0..FIXED32_SIZE].try_into().expect("length checked"))
                    as usize;
            if old_style != *actual_size
                && old_style < file_size
                && old_style + FIXED32_SIZE <= file_size
                && crc32(0, &data[FIXED32_SIZE..FIXED32_SIZE + old_style]) == meta.crc_digest
            {
                mmkv_log!(
                    crate::handler::Level::Info,
                    "[{}] looks like a downgrade & upgrade, using old actual size {}",
                    id,
                    old_style
                );
                *actual_size = old_style;
                return (true, false);
            }
        }

        if *actual_size < file_size && *actual_size + FIXED32_SIZE <= file_size {
            if crc32(0, &data[FIXED32_SIZE..FIXED32_SIZE + *actual_size]) == meta.crc_digest {
                return (true, false);
            }

            if let Some(last) = last_confirmed(data, file_size, meta) {
                *actual_size = last;
                return (true, false);
            }

            let strategy = strategy_for_crc(recover, id);
            mmkv_log!(
                crate::handler::Level::Error,
                "[{}] CRC check failed, strategy {:?}",
                id,
                strategy
            );
            if strategy == Recover::Recover {
                return (true, true);
            }
            return (false, false);
        }

        mmkv_log!(
            crate::handler::Level::Error,
            "[{}] actual size {} exceeds file size {}",
            id,
            *actual_size,
            file_size
        );
        if let Some(last) = last_confirmed(data, file_size, meta) {
            *actual_size = last;
            return (true, false);
        }

        let strategy = strategy_for_length(recover, id);
        if strategy == Recover::Recover {
            *actual_size = file_size - FIXED32_SIZE;
            return (true, true);
        }
        (false, false)
    }

    fn ensure_loaded_locked(&self, inner: &mut Inner) -> Result<()> {
        if inner.closed {
            return Err(Error::Closed);
        }
        if inner.need_load {
            let _guard = self.lock.shared_guard()?;
            self.load_locked(inner)?;
            return Ok(());
        }
        if !inner.multi_process {
            return Ok(());
        }

        let current = MetaInfo::read(inner.meta.bytes()).ok_or_else(|| Error::FileNotValid {
            path: self.crc_path.to_string_lossy().into_owned(),
        })?;

        if current.sequence != inner.meta_info.sequence {
            let _guard = self.lock.shared_guard()?;
            mmkv_log!(
                crate::handler::Level::Info,
                "[{}] sequence changed {} -> {}",
                self.id,
                inner.meta_info.sequence,
                current.sequence
            );
            inner.dict.clear();
            inner.actual_size = 0;
            inner.output_position = 0;
            inner.need_load = true;
            self.load_locked(inner)?;
            drop(_guard);
            self.notify_content_changed();
        } else if current.crc_digest != inner.meta_info.crc_digest
            || current.actual_size != inner.meta_info.actual_size
        {
            let _guard = self.lock.shared_guard()?;
            self.partial_load_locked(inner, &current)?;
            drop(_guard);
            self.notify_content_changed();
        }

        Ok(())
    }

    fn partial_load_locked(&self, inner: &mut Inner, current: &MetaInfo) -> Result<()> {
        let old_actual = inner.actual_size;
        let file_size = inner
            .data
            .as_ref()
            .map(MemoryFile::size)
            .unwrap_or_default();
        let new_actual = if current.version >= meta::VERSION_ACTUAL_SIZE {
            current.actual_size as usize
        } else if file_size >= FIXED32_SIZE {
            let bytes = inner.data.as_ref().expect("data exists").bytes();
            u32::from_le_bytes(bytes[0..4].try_into().expect("length checked")) as usize
        } else {
            0
        };

        if new_actual > old_actual && new_actual + FIXED32_SIZE <= file_size {
            let matches = {
                let bytes = inner.data.as_ref().expect("data exists").bytes();
                crc32(
                    inner.crc_digest,
                    &bytes[FIXED32_SIZE + old_actual..FIXED32_SIZE + new_actual],
                ) == current.crc_digest
            };
            if matches {
                let crypt = if let Some(crypter) = inner.crypter.as_ref() {
                    let bytes = inner.data.as_ref().expect("data exists").bytes();
                    let payload = &bytes[FIXED32_SIZE..FIXED32_SIZE + new_actual];
                    let entries = decode_entries_crypt(payload, old_actual, true, crypter)?;
                    let status = crypter.status_at(payload, new_actual);
                    Some((entries, status))
                } else {
                    None
                };
                if let Some((entries, status)) = crypt {
                    apply_crypt_entries(&mut inner.dict_crypt, entries);
                    if let Some(crypter) = inner.crypter.as_mut() {
                        crypter.set_status(&status);
                    }
                } else {
                    let entries = {
                        let bytes = inner.data.as_ref().expect("data exists").bytes();
                        decode_entries(
                            &bytes[FIXED32_SIZE..FIXED32_SIZE + new_actual],
                            old_actual,
                            true,
                        )?
                    };
                    apply_entries(&mut inner.dict, entries);
                }
                inner.actual_size = new_actual;
                inner.output_position = new_actual;
                inner.crc_digest = current.crc_digest;
                inner.meta_info = current.clone();
                inner.has_full_writeback = false;
                return Ok(());
            }
            mmkv_log!(
                crate::handler::Level::Error,
                "[{}] partial load CRC mismatch",
                self.id
            );
        }

        inner.dict.clear();
        inner.dict_crypt.clear();
        inner.actual_size = 0;
        inner.output_position = 0;
        inner.need_load = true;
        self.load_locked(inner)
    }

    fn notify_content_changed(&self) {
        if let Some(handler) = handler() {
            handler.on_content_changed(&self.id);
        }
    }

    fn notify_content_loaded(&self) {
        if let Some(handler) = handler() {
            handler.on_content_loaded(&self.id);
        }
    }

    // ── metadata writes ─────────────────────────────────────────────────

    fn write_actual_size_locked(
        &self,
        inner: &mut Inner,
        size: usize,
        crc_digest: u32,
        increase_sequence: bool,
        iv: Option<&[u8; AES_IV_LEN]>,
    ) -> Result<()> {
        if inner.read_only {
            return Err(Error::ReadOnly);
        }

        if !increase_sequence
            && inner.meta_info.version < meta::VERSION_ACTUAL_SIZE
            && let Some(data) = inner.data.as_mut()
            && data.size() >= FIXED32_SIZE
        {
            let bytes = data.bytes_mut();
            bytes[0..4].copy_from_slice(&(size as u32).to_le_bytes());
        }

        let mut info = inner.meta_info.clone();
        inner.actual_size = size;
        inner.crc_digest = crc_digest;
        info.actual_size = size as u32;
        info.crc_digest = crc_digest;

        let mut needs_full_write = false;
        if let Some(iv) = iv {
            info.vector = *iv;
            if info.version < meta::VERSION_RANDOM_IV {
                info.version = meta::VERSION_RANDOM_IV;
            }
            needs_full_write = true;
        }
        if info.version < meta::VERSION_SEQUENCE {
            info.version = meta::VERSION_SEQUENCE;
            needs_full_write = true;
        }
        if increase_sequence {
            info.sequence = info.sequence.wrapping_add(1);
            info.last_actual_size = size as u32;
            info.last_crc_digest = crc_digest;
            if info.version < meta::VERSION_ACTUAL_SIZE {
                info.version = meta::VERSION_ACTUAL_SIZE;
            }
            needs_full_write = true;
        }
        if info.version < meta::VERSION_FLAG {
            info.flags = 0;
            info.version = meta::VERSION_FLAG;
            needs_full_write = true;
        }

        let buffer = inner.meta.bytes_mut();
        if needs_full_write {
            info.write(buffer);
        } else {
            info.write_crc_and_size(buffer);
        }
        inner.meta_info = info;
        Ok(())
    }

    fn sync_locked(&self, inner: &mut Inner, mode: SyncMode) -> Result<()> {
        if inner.need_load {
            return Ok(());
        }
        if let Some(data) = inner.data.as_ref() {
            data.sync(mode)?;
        }
        inner.meta.sync(mode)?;
        Ok(())
    }

    // ── writes ──────────────────────────────────────────────────────────

    fn put_blob(
        &self,
        key: &str,
        blob: &[u8],
        data_holder: bool,
        expire_duration: Option<u32>,
    ) -> Result<()> {
        if key.is_empty() {
            return Err(Error::InvalidId(key.to_owned()));
        }
        let mut inner = self.inner.lock();
        if inner.closed {
            return Err(Error::Closed);
        }
        if inner.read_only {
            return Err(Error::ReadOnly);
        }
        // Acquire the exclusive process lock *before* refreshing from disk, so
        // another process cannot append between the refresh and our write.
        let _guard = self.lock.exclusive_guard()?;
        self.ensure_loaded_locked(&mut inner)?;
        self.put_blob_locked(
            &mut inner,
            key.as_bytes(),
            blob,
            data_holder,
            expire_duration,
        )
    }

    fn put_blob_locked(
        &self,
        inner: &mut Inner,
        key: &[u8],
        blob: &[u8],
        data_holder: bool,
        expire_duration: Option<u32>,
    ) -> Result<()> {
        if key.is_empty() {
            return Err(Error::InvalidId("<empty>".to_owned()));
        }
        if inner.expiration_enabled {
            let duration = match expire_duration {
                Some(seconds) => seconds,
                None => inner.expired_in_seconds,
            };
            let time = if duration != 0 {
                self.platform.now_unix_seconds().saturating_add(duration)
            } else {
                0
            };
            let mut tmp = Vec::with_capacity(blob.len() + 8);
            if data_holder {
                crate::codec::varint::append_varint32(&mut tmp, blob.len() as u32);
                tmp.extend_from_slice(blob);
            } else {
                tmp.extend_from_slice(blob);
            }
            tmp.extend_from_slice(&time.to_le_bytes());
            return self.set_blob_locked(inner, key, &tmp, false);
        }
        self.set_blob_locked(inner, key, blob, data_holder)
    }

    fn set_blob_locked(
        &self,
        inner: &mut Inner,
        key: &[u8],
        blob: &[u8],
        data_holder: bool,
    ) -> Result<()> {
        if inner.read_only {
            return Err(Error::ReadOnly);
        }
        if key.is_empty() {
            return Err(Error::InvalidId("<empty>".to_owned()));
        }
        if inner.crypter.is_some() {
            return self.set_crypt_blob_locked(inner, key, blob, data_holder);
        }
        let (value_len, total) = encoded_entry(key, blob, data_holder)?;

        if inner.item_size_limit != 0 && total > inner.item_size_limit as usize {
            return Err(Error::ItemSizeLimit {
                size: total,
                limit: inner.item_size_limit,
            });
        }

        if let Some(existing) = inner.dict.get(key).copied() {
            if inner.compare_before_set && !inner.expiration_enabled && inner.crypter.is_none() {
                let equal = self
                    .value_for_inner(inner, existing)
                    .map(|old| equal_encoded(&old, blob, data_holder))
                    .unwrap_or(false);
                if equal {
                    return Ok(());
                }
            }

            let only_one = !inner.multi_process && inner.dict.len() == 1;
            if only_one && self.check_size_for_override(inner, total) {
                self.override_locked(inner, key, blob, data_holder, total)?;
                inner.has_full_writeback = false;
                return Ok(());
            }
            self.append_locked(inner, key, blob, data_holder, value_len, total)?;
        } else {
            let override_empty =
                !inner.multi_process && inner.dict.is_empty() && inner.actual_size > 0;
            if override_empty && self.check_size_for_override(inner, total) {
                self.override_locked(inner, key, blob, data_holder, total)?;
            } else {
                self.append_locked(inner, key, blob, data_holder, value_len, total)?;
            }
        }
        inner.has_full_writeback = false;
        Ok(())
    }

    /// Encrypted append path. MMKV never rewrites in place while encryption is
    /// enabled: it appends and rotates the IV on compaction.
    fn set_crypt_blob_locked(
        &self,
        inner: &mut Inner,
        key: &[u8],
        blob: &[u8],
        data_holder: bool,
    ) -> Result<()> {
        let mut encoded_blob = None;
        let mut data_holder = data_holder;
        if data_holder {
            let size_needed = blob.len() + varint_size_u32(blob.len() as u32);
            if !KeyValueHolderCrypt::is_value_stored_as_offset(size_needed) {
                let mut encoded = Vec::with_capacity(size_needed);
                crate::codec::varint::append_varint32(&mut encoded, blob.len() as u32);
                encoded.extend_from_slice(blob);
                encoded_blob = Some(encoded);
                data_holder = false;
            }
        }
        let blob = encoded_blob.as_deref().unwrap_or(blob);
        let (value_len, total) = encoded_entry(key, blob, data_holder)?;

        if inner.item_size_limit != 0 && total > inner.item_size_limit as usize {
            return Err(Error::ItemSizeLimit {
                size: total,
                limit: inner.item_size_limit,
            });
        }

        self.ensure_memory_size_locked(inner, total)?;

        let offset = inner.actual_size;
        let status = inner.crypter.as_ref().expect("crypter exists").status();

        let mut entry = vec![0u8; total];
        let written = write_entry_at(&mut entry, 0, key, blob, data_holder)?;
        debug_assert_eq!(written, total);
        inner
            .crypter
            .as_mut()
            .expect("crypter exists")
            .encrypt(&mut entry);

        {
            let data = inner.data.as_mut().ok_or_else(|| Error::FileNotValid {
                path: self.path.to_string_lossy().into_owned(),
            })?;
            let buffer = data.bytes_mut();
            buffer[FIXED32_SIZE + offset..FIXED32_SIZE + offset + total].copy_from_slice(&entry);
        }

        let crc_digest = {
            let data = inner.data.as_ref().expect("data exists");
            let bytes = data.bytes();
            crc32(
                inner.crc_digest,
                &bytes[FIXED32_SIZE + offset..FIXED32_SIZE + offset + total],
            )
        };

        let new_actual = offset + total;
        inner.actual_size = new_actual;
        inner.output_position = new_actual;

        if value_len == 0 {
            inner.dict_crypt.remove(key);
        } else {
            let value_blob = if data_holder {
                let mut value = Vec::with_capacity(blob.len() + varint_size_u32(blob.len() as u32));
                crate::codec::varint::append_varint32(&mut value, blob.len() as u32);
                value.extend_from_slice(blob);
                value
            } else {
                blob.to_vec()
            };
            let holder = if KeyValueHolderCrypt::is_value_stored_as_offset(value_len as usize) {
                KeyValueHolderCrypt::offset(key.len() as u32, value_len, offset as u32, status)
                    .ok_or_else(|| Error::TooLarge {
                        key: String::from_utf8_lossy(key).into_owned(),
                        size: total,
                    })?
            } else {
                KeyValueHolderCrypt::from_value(&value_blob)
            };
            inner
                .dict_crypt
                .insert(key.to_vec().into_boxed_slice(), holder);
        }

        self.write_actual_size_locked(inner, new_actual, crc_digest, false, None)?;
        inner.has_full_writeback = false;
        Ok(())
    }

    fn check_size_for_override(&self, inner: &Inner, total: usize) -> bool {
        let Some(data) = inner.data.as_ref() else {
            return false;
        };
        total + FIXED32_SIZE + ITEM_SIZE_HOLDER_SIZE <= data.size()
    }

    fn override_locked(
        &self,
        inner: &mut Inner,
        key: &[u8],
        blob: &[u8],
        data_holder: bool,
        total: usize,
    ) -> Result<()> {
        let data = inner.data.as_mut().ok_or_else(|| Error::FileNotValid {
            path: self.path.to_string_lossy().into_owned(),
        })?;
        {
            let buffer = data.bytes_mut();
            let mut output = CodedOutput::new(&mut buffer[FIXED32_SIZE..]);
            output.write_varint32(FAKE_MAP_SIZE)?;
            let written = write_entry_at(
                buffer,
                FIXED32_SIZE + ITEM_SIZE_HOLDER_SIZE,
                key,
                blob,
                data_holder,
            )?;
            debug_assert_eq!(written, total);
        }
        inner.actual_size = ITEM_SIZE_HOLDER_SIZE + total;
        inner.output_position = inner.actual_size;

        let value_len = encoded_value_length(blob, data_holder)?;
        let holder = KeyValueHolder::new(key.len() as u32, value_len, ITEM_SIZE_HOLDER_SIZE as u32)
            .ok_or_else(|| Error::TooLarge {
                key: String::from_utf8_lossy(key).into_owned(),
                size: total,
            })?;
        inner.dict.clear();
        inner.dict.insert(key.to_vec().into_boxed_slice(), holder);

        let crc_digest = {
            let bytes = data.bytes();
            crc32(0, &bytes[FIXED32_SIZE..FIXED32_SIZE + inner.actual_size])
        };
        self.write_actual_size_locked(inner, inner.actual_size, crc_digest, false, None)?;
        Ok(())
    }

    fn append_locked(
        &self,
        inner: &mut Inner,
        key: &[u8],
        blob: &[u8],
        data_holder: bool,
        value_len: u32,
        total: usize,
    ) -> Result<()> {
        self.ensure_memory_size_locked(inner, total)?;
        let offset = inner.actual_size;
        let data = inner.data.as_mut().ok_or_else(|| Error::FileNotValid {
            path: self.path.to_string_lossy().into_owned(),
        })?;

        let crc_digest = {
            let buffer = data.bytes_mut();
            let start = FIXED32_SIZE + offset;
            let written = write_entry_at(buffer, start, key, blob, data_holder)?;
            debug_assert_eq!(written, total);
            let bytes = data.bytes();
            crc32(inner.crc_digest, &bytes[start..start + total])
        };

        let new_actual = offset + total;
        inner.actual_size = new_actual;
        inner.output_position = new_actual;

        if value_len == 0 {
            inner.dict.remove(key);
        } else {
            let holder = KeyValueHolder::new(key.len() as u32, value_len, offset as u32)
                .ok_or_else(|| Error::TooLarge {
                    key: String::from_utf8_lossy(key).into_owned(),
                    size: total,
                })?;
            inner.dict.insert(key.to_vec().into_boxed_slice(), holder);
        }

        self.write_actual_size_locked(inner, new_actual, crc_digest, false, None)?;
        Ok(())
    }

    fn ensure_memory_size_locked(&self, inner: &mut Inner, new_size: usize) -> Result<()> {
        if inner.read_only {
            return Err(Error::ReadOnly);
        }
        let Some(data) = inner.data.as_ref() else {
            return Err(Error::FileNotValid {
                path: self.path.to_string_lossy().into_owned(),
            });
        };

        let maximum_actual_size = u32::MAX as usize;
        if new_size > maximum_actual_size {
            return Err(Error::TooLarge {
                key: "<incoming>".to_owned(),
                size: new_size,
            });
        }
        let exceeds = inner.actual_size > maximum_actual_size - new_size;
        let file_size = data.size();
        let space_left = file_size.saturating_sub(FIXED32_SIZE + inner.actual_size);
        let empty = Self::dictionary_is_empty(inner);

        if new_size >= space_left || empty || exceeds {
            self.filter_expired_locked(inner);
            let prepared = self.prepared_size(inner);
            if prepared > maximum_actual_size - new_size {
                return Err(Error::TooLarge {
                    key: "<compacted>".to_owned(),
                    size: prepared,
                });
            }
            let need_sync = !Self::dictionary_is_empty(inner);
            return self.expand_and_write_back_locked(inner, new_size, prepared, need_sync);
        }
        Ok(())
    }

    fn dictionary_is_empty(inner: &Inner) -> bool {
        if inner.crypter.is_some() {
            inner.dict_crypt.is_empty()
        } else {
            inner.dict.is_empty()
        }
    }

    fn dictionary_len(inner: &Inner) -> usize {
        if inner.crypter.is_some() {
            inner.dict_crypt.len()
        } else {
            inner.dict.len()
        }
    }

    fn prepared_size(&self, inner: &Inner) -> usize {
        if inner.crypter.is_some() {
            let mut total = ITEM_SIZE_HOLDER_SIZE;
            for (key, holder) in &inner.dict_crypt {
                let value_len = holder.real_value_size() as usize;
                total += varint_size_u32(key.len() as u32)
                    + key.len()
                    + varint_size_u32(value_len as u32)
                    + value_len;
            }
            return total;
        }
        ITEM_SIZE_HOLDER_SIZE
            + inner
                .dict
                .values()
                .map(|holder| holder.computed_kv_size as usize + holder.value_size as usize)
                .sum::<usize>()
    }

    fn expand_and_write_back_locked(
        &self,
        inner: &mut Inner,
        new_size: usize,
        prepared_size: usize,
        need_sync: bool,
    ) -> Result<()> {
        let mut file_size = inner
            .data
            .as_ref()
            .map(MemoryFile::size)
            .unwrap_or_default();
        let len_needed = prepared_size
            .checked_add(FIXED32_SIZE)
            .and_then(|value| value.checked_add(new_size))
            .ok_or(Error::TooLarge {
                key: "<incoming>".to_owned(),
                size: new_size,
            })?;

        let now_count = Self::dictionary_len(inner);
        let later_count = (now_count + 1).max(1);
        let average_item = len_needed / later_count + usize::from(len_needed % later_count != 0);
        let future_items = (later_count / 2).max(8);
        let future_usage = average_item.saturating_mul(future_items);
        let expected_usage = len_needed.saturating_add(future_usage);

        if len_needed >= file_size || (need_sync && expected_usage >= file_size) {
            let old_size = file_size;
            loop {
                if file_size > usize::MAX / 2 {
                    file_size = file_size.max(len_needed);
                    break;
                }
                file_size *= 2;
                if expected_usage < file_size {
                    break;
                }
            }
            mmkv_log!(
                crate::handler::Level::Info,
                "[{}] extending file from {} to {}, incoming {}",
                self.id,
                old_size,
                file_size,
                new_size
            );
            inner
                .data
                .as_mut()
                .expect("data exists")
                .resize(file_size)?;
        }

        self.do_full_write_back_locked(inner, need_sync)
    }

    fn do_full_write_back_locked(&self, inner: &mut Inner, need_sync: bool) -> Result<()> {
        if inner.crypter.is_some() {
            self.do_full_write_back_crypt_locked(inner, need_sync)
        } else {
            self.do_full_write_back_plain_locked(inner, need_sync)
        }
    }

    fn do_full_write_back_plain_locked(&self, inner: &mut Inner, need_sync: bool) -> Result<()> {
        let mut items: Vec<(Box<[u8]>, KeyValueHolder)> = inner
            .dict
            .iter()
            .map(|(key, holder)| (key.clone(), *holder))
            .collect();
        items.sort_by_key(|(_, holder)| holder.offset);

        let total = ITEM_SIZE_HOLDER_SIZE
            + items
                .iter()
                .map(|(_, holder)| holder.computed_kv_size as usize + holder.value_size as usize)
                .sum::<usize>();
        let file_size = inner
            .data
            .as_ref()
            .map(MemoryFile::size)
            .unwrap_or_default();
        if total + FIXED32_SIZE > file_size {
            return Err(Error::TooLarge {
                key: "<compacted>".to_owned(),
                size: total,
            });
        }

        let crc_digest = {
            let data = inner.data.as_mut().expect("data exists");
            let buffer = data.bytes_mut();
            {
                let mut output = CodedOutput::new(
                    &mut buffer[FIXED32_SIZE..FIXED32_SIZE + ITEM_SIZE_HOLDER_SIZE],
                );
                output.write_varint32(FAKE_MAP_SIZE)?;
            }

            let mut sections: Vec<(usize, usize)> = Vec::new();
            for (_, holder) in &items {
                let offset = holder.offset as usize;
                let size = holder.computed_kv_size as usize + holder.value_size as usize;
                if let Some(last) = sections.last_mut()
                    && offset == last.0 + last.1
                {
                    last.1 += size;
                    continue;
                }
                sections.push((offset, size));
            }

            let mut write = ITEM_SIZE_HOLDER_SIZE;
            for (offset, size) in sections {
                let source = FIXED32_SIZE + offset;
                let destination = FIXED32_SIZE + write;
                buffer.copy_within(source..source + size, destination);
                write += size;
            }
            debug_assert_eq!(write, total);

            crc32(0, &buffer[FIXED32_SIZE..FIXED32_SIZE + total])
        };

        let mut offset = ITEM_SIZE_HOLDER_SIZE as u32;
        for (key, mut holder) in items {
            holder.offset = offset;
            offset += holder.computed_kv_size as u32 + holder.value_size;
            inner.dict.insert(key, holder);
        }

        inner.actual_size = total;
        inner.output_position = total;
        self.write_actual_size_locked(inner, total, crc_digest, true, None)?;
        inner.has_full_writeback = true;
        if need_sync {
            self.sync_locked(inner, SyncMode::Sync)?;
        }
        Ok(())
    }

    fn do_full_write_back_crypt_locked(&self, inner: &mut Inner, need_sync: bool) -> Result<()> {
        let entries = {
            let data = inner.data.as_ref().ok_or_else(|| Error::FileNotValid {
                path: self.path.to_string_lossy().into_owned(),
            })?;
            let payload = data
                .bytes()
                .get(FIXED32_SIZE..)
                .ok_or(Error::Corrupted(CorruptKind::Length))?;
            let crypter = inner
                .crypter
                .as_ref()
                .ok_or(Error::Unsupported("encryption is not enabled"))?;
            let mut entries = Vec::with_capacity(inner.dict_crypt.len());
            for (key, holder) in &inner.dict_crypt {
                let value = holder
                    .value(payload, crypter)
                    .ok_or(Error::Corrupted(CorruptKind::Length))?;
                entries.push((key.clone(), value));
            }
            entries
        };
        self.write_all_entries_locked(inner, entries, need_sync)
    }

    /// Rewrite the whole store from plaintext value blobs. Used by encryption
    /// full write-back, `rekey`, and expiration migration. A fresh IV is
    /// generated whenever encryption is enabled.
    fn write_all_entries_locked(
        &self,
        inner: &mut Inner,
        mut entries: RawEntries,
        need_sync: bool,
    ) -> Result<()> {
        entries.sort_by(|left, right| left.0.cmp(&right.0));

        let mut total = ITEM_SIZE_HOLDER_SIZE;
        for (key, value) in &entries {
            let (_, entry_size) = encoded_entry(key, value, false)?;
            total = total.checked_add(entry_size).ok_or(Error::TooLarge {
                key: "<compacted>".to_owned(),
                size: total,
            })?;
        }
        self.ensure_capacity_locked(inner, total)?;

        let encrypted = inner.crypter.is_some();
        let mut iv = [0u8; AES_IV_LEN];
        if encrypted {
            self.platform.random_bytes(&mut iv)?;
            inner
                .crypter
                .as_mut()
                .expect("crypter exists")
                .reset_iv(Some(&iv));
        }

        // Placeholder, encrypted with the same stream as the entries.
        let mut placeholder = [0u8; ITEM_SIZE_HOLDER_SIZE];
        {
            let mut output = CodedOutput::new(&mut placeholder);
            output.write_varint32(FAKE_MAP_SIZE)?;
        }
        if encrypted {
            inner
                .crypter
                .as_mut()
                .expect("crypter exists")
                .encrypt(&mut placeholder);
        }
        inner
            .data
            .as_mut()
            .ok_or_else(|| Error::FileNotValid {
                path: self.path.to_string_lossy().into_owned(),
            })?
            .bytes_mut()[FIXED32_SIZE..FIXED32_SIZE + ITEM_SIZE_HOLDER_SIZE]
            .copy_from_slice(&placeholder);

        let mut new_dict = Dictionary::new();
        let mut new_dict_crypt = CryptDictionary::new();
        let mut offset = ITEM_SIZE_HOLDER_SIZE as u32;
        for (key, value) in entries {
            let value_len = value.len() as u32;
            let (_, entry_size) = encoded_entry(&key, &value, false)?;
            let status = if encrypted {
                Some(inner.crypter.as_ref().expect("crypter exists").status())
            } else {
                None
            };

            let mut entry = vec![0u8; entry_size];
            write_entry_at(&mut entry, 0, &key, &value, false)?;
            if encrypted {
                inner
                    .crypter
                    .as_mut()
                    .expect("crypter exists")
                    .encrypt(&mut entry);
            }
            {
                let data = inner.data.as_mut().expect("data exists");
                let start = FIXED32_SIZE + offset as usize;
                data.bytes_mut()[start..start + entry_size].copy_from_slice(&entry);
            }

            if let Some(status) = status {
                let holder = if KeyValueHolderCrypt::is_value_stored_as_offset(value_len as usize) {
                    KeyValueHolderCrypt::offset(key.len() as u32, value_len, offset, status)
                        .ok_or_else(|| Error::TooLarge {
                            key: String::from_utf8_lossy(&key).into_owned(),
                            size: value_len as usize,
                        })?
                } else {
                    KeyValueHolderCrypt::from_value(&value)
                };
                new_dict_crypt.insert(key, holder);
            } else {
                let holder =
                    KeyValueHolder::new(key.len() as u32, value_len, offset).ok_or_else(|| {
                        Error::TooLarge {
                            key: String::from_utf8_lossy(&key).into_owned(),
                            size: value_len as usize,
                        }
                    })?;
                new_dict.insert(key, holder);
            }
            offset += entry_size as u32;
        }
        debug_assert_eq!(offset as usize, total);

        let crc_digest = {
            let data = inner.data.as_ref().expect("data exists");
            let bytes = data.bytes();
            crc32(0, &bytes[FIXED32_SIZE..FIXED32_SIZE + total])
        };
        inner.dict = new_dict;
        inner.dict_crypt = new_dict_crypt;
        inner.actual_size = total;
        inner.output_position = total;
        let iv = if encrypted { Some(&iv) } else { None };
        self.write_actual_size_locked(inner, total, crc_digest, true, iv)?;
        inner.has_full_writeback = true;
        if need_sync {
            self.sync_locked(inner, SyncMode::Sync)?;
        }
        Ok(())
    }

    fn ensure_capacity_locked(&self, inner: &mut Inner, total: usize) -> Result<()> {
        let file_size = inner
            .data
            .as_ref()
            .map(MemoryFile::size)
            .unwrap_or_default();
        if total + FIXED32_SIZE <= file_size {
            return Ok(());
        }
        let needed = total + FIXED32_SIZE;
        let mut new_size = file_size.max(self.platform.page_size());
        while new_size < needed {
            new_size = new_size.checked_mul(2).ok_or(Error::TooLarge {
                key: "<compacted>".to_owned(),
                size: needed,
            })?;
        }
        inner.data.as_mut().expect("data exists").resize(new_size)?;
        Ok(())
    }

    fn full_writeback_locked(&self, inner: &mut Inner) -> Result<()> {
        if inner.has_full_writeback || inner.need_load {
            return Ok(());
        }
        if inner.read_only {
            return Err(Error::ReadOnly);
        }
        let _guard = self.lock.exclusive_guard()?;
        if inner.expiration_enabled {
            self.filter_expired_locked(inner);
        }
        if Self::dictionary_is_empty(inner) {
            return self.clear_all_locked(inner, false);
        }

        let prepared = self.prepared_size(inner);
        let file_size = inner
            .data
            .as_ref()
            .map(MemoryFile::size)
            .unwrap_or_default();
        if prepared + FIXED32_SIZE <= file_size {
            self.do_full_write_back_locked(inner, true)
        } else {
            let new_size = prepared + FIXED32_SIZE - file_size;
            self.expand_and_write_back_locked(inner, new_size, prepared, true)
        }
    }

    // ── removal / clearing ──────────────────────────────────────────────

    fn remove_locked(&self, inner: &mut Inner, key: &[u8]) -> Result<bool> {
        if inner.read_only {
            return Err(Error::ReadOnly);
        }
        if inner.crypter.is_some() {
            if !inner.dict_crypt.contains_key(key) {
                return Ok(false);
            }
            self.set_crypt_blob_locked(inner, key, &[], false)?;
            return Ok(true);
        }
        if !inner.dict.contains_key(key) {
            return Ok(false);
        }
        let (value_len, total) = encoded_entry(key, &[], false)?;
        debug_assert_eq!(value_len, 0);
        self.ensure_memory_size_locked(inner, total)?;
        let offset = inner.actual_size;
        let data = inner.data.as_mut().ok_or_else(|| Error::FileNotValid {
            path: self.path.to_string_lossy().into_owned(),
        })?;
        let crc_digest = {
            let buffer = data.bytes_mut();
            let start = FIXED32_SIZE + offset;
            let written = write_entry_at(buffer, start, key, &[], false)?;
            debug_assert_eq!(written, total);
            let bytes = data.bytes();
            crc32(inner.crc_digest, &bytes[start..start + total])
        };
        let new_actual = offset + total;
        inner.actual_size = new_actual;
        inner.output_position = new_actual;
        inner.dict.remove(key);
        self.write_actual_size_locked(inner, new_actual, crc_digest, false, None)?;
        inner.has_full_writeback = false;
        Ok(true)
    }

    fn clear_all_locked(&self, inner: &mut Inner, keep_space: bool) -> Result<()> {
        if inner.read_only {
            return Err(Error::ReadOnly);
        }
        let file_size = inner
            .data
            .as_ref()
            .map(MemoryFile::size)
            .unwrap_or_default();
        if file_size == inner.expected_capacity && inner.actual_size == 0 {
            return Ok(());
        }

        if !keep_space {
            inner
                .data
                .as_mut()
                .expect("data exists")
                .resize(inner.expected_capacity)?;
        }

        let mut iv = [0u8; AES_IV_LEN];
        let iv_ref = if let Some(crypter) = inner.crypter.as_mut() {
            self.platform.random_bytes(&mut iv)?;
            crypter.reset_iv(Some(&iv));
            Some(&iv)
        } else {
            None
        };

        inner.actual_size = 0;
        inner.output_position = 0;
        self.write_actual_size_locked(inner, 0, 0, true, iv_ref)?;
        inner.meta.sync(SyncMode::Sync)?;

        inner.dict.clear();
        inner.dict_crypt.clear();
        inner.crc_digest = 0;
        inner.need_load = true;
        if !keep_space {
            inner.data = None;
        }
        self.load_locked(inner)
    }

    fn clear_memory_cache_locked(&self, inner: &mut Inner, keep_space: bool) {
        inner.dict.clear();
        inner.dict_crypt.clear();
        inner.actual_size = 0;
        inner.output_position = 0;
        inner.crc_digest = 0;
        inner.need_load = true;
        inner.has_full_writeback = false;
        if !keep_space {
            inner.data = None;
        }
    }

    // ── public reads ────────────────────────────────────────────────────

    fn with_shared<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> Result<R> {
        self.touch();
        let mut inner = self.inner.lock();
        if inner.closed {
            return Err(Error::Closed);
        }
        let _guard = self.lock.shared_guard()?;
        self.ensure_loaded_locked(&mut inner)?;
        Ok(f(&mut inner))
    }

    fn with_exclusive<R>(&self, f: impl FnOnce(&mut Inner) -> Result<R>) -> Result<R> {
        self.touch();
        let mut inner = self.inner.lock();
        if inner.closed {
            return Err(Error::Closed);
        }
        if inner.read_only {
            return Err(Error::ReadOnly);
        }
        let _guard = self.lock.exclusive_guard()?;
        self.ensure_loaded_locked(&mut inner)?;
        f(&mut inner)
    }

    fn value_for_inner(&self, inner: &Inner, holder: KeyValueHolder) -> Option<Vec<u8>> {
        let data = inner.data.as_ref()?;
        let payload = data.bytes().get(FIXED32_SIZE..)?;
        holder.value(payload).map(|value| value.to_vec())
    }

    /// Value blob exactly as stored, without expiration stripping.
    fn value_blob_raw(&self, inner: &Inner, key: &[u8]) -> Option<Vec<u8>> {
        if let Some(crypter) = inner.crypter.as_ref() {
            let holder = inner.dict_crypt.get(key)?;
            let data = inner.data.as_ref()?;
            let payload = data.bytes().get(FIXED32_SIZE..)?;
            holder.value(payload, crypter)
        } else {
            let holder = *inner.dict.get(key)?;
            self.value_for_inner(inner, holder)
        }
    }

    /// Read a value and delete it immediately when it has expired (MMKV's
    /// `getDataWithoutMTimeForKey` appends a tombstone on read).
    fn raw_blob_mut(&self, inner: &mut Inner, key: &[u8]) -> Option<Vec<u8>> {
        let value = self.value_blob_raw(inner, key)?;
        if inner.expiration_enabled {
            if value.len() < FIXED32_SIZE {
                return None;
            }
            let time =
                u32::from_le_bytes(value[value.len() - 4..].try_into().expect("length checked"));
            if time != 0 && time <= self.platform.now_unix_seconds() {
                // Upgrade the shared lock to exclusive before writing a tombstone.
                let _guard = self.lock.exclusive_guard().ok()?;
                let _ = self.remove_locked(inner, key);
                return None;
            }
            return Some(value[..value.len() - FIXED32_SIZE].to_vec());
        }
        Some(value)
    }

    pub fn get_bool(&self, key: &str, default: bool) -> bool {
        self.get_bool_opt(key).unwrap_or(default)
    }

    pub fn get_bool_opt(&self, key: &str) -> Option<bool> {
        let blob = self
            .with_shared(|inner| self.raw_blob_mut(inner, key.as_bytes()))
            .ok()
            .flatten()?;
        mini_pb::decode_bool(&blob).ok()
    }

    pub fn get_i32(&self, key: &str, default: i32) -> i32 {
        self.get_i32_opt(key).unwrap_or(default)
    }

    pub fn get_i32_opt(&self, key: &str) -> Option<i32> {
        let blob = self
            .with_shared(|inner| self.raw_blob_mut(inner, key.as_bytes()))
            .ok()
            .flatten()?;
        mini_pb::decode_i32(&blob).ok()
    }

    pub fn get_u32(&self, key: &str, default: u32) -> u32 {
        self.get_u32_opt(key).unwrap_or(default)
    }

    pub fn get_u32_opt(&self, key: &str) -> Option<u32> {
        let blob = self
            .with_shared(|inner| self.raw_blob_mut(inner, key.as_bytes()))
            .ok()
            .flatten()?;
        mini_pb::decode_u32(&blob).ok()
    }

    pub fn get_i64(&self, key: &str, default: i64) -> i64 {
        self.get_i64_opt(key).unwrap_or(default)
    }

    pub fn get_i64_opt(&self, key: &str) -> Option<i64> {
        let blob = self
            .with_shared(|inner| self.raw_blob_mut(inner, key.as_bytes()))
            .ok()
            .flatten()?;
        mini_pb::decode_i64(&blob).ok()
    }

    pub fn get_u64(&self, key: &str, default: u64) -> u64 {
        self.get_u64_opt(key).unwrap_or(default)
    }

    pub fn get_u64_opt(&self, key: &str) -> Option<u64> {
        let blob = self
            .with_shared(|inner| self.raw_blob_mut(inner, key.as_bytes()))
            .ok()
            .flatten()?;
        mini_pb::decode_u64(&blob).ok()
    }

    pub fn get_f32(&self, key: &str, default: f32) -> f32 {
        self.get_f32_opt(key).unwrap_or(default)
    }

    pub fn get_f32_opt(&self, key: &str) -> Option<f32> {
        let blob = self
            .with_shared(|inner| self.raw_blob_mut(inner, key.as_bytes()))
            .ok()
            .flatten()?;
        mini_pb::decode_f32(&blob).ok()
    }

    pub fn get_f64(&self, key: &str, default: f64) -> f64 {
        self.get_f64_opt(key).unwrap_or(default)
    }

    pub fn get_f64_opt(&self, key: &str) -> Option<f64> {
        let blob = self
            .with_shared(|inner| self.raw_blob_mut(inner, key.as_bytes()))
            .ok()
            .flatten()?;
        mini_pb::decode_f64(&blob).ok()
    }

    pub fn get_str(&self, key: &str) -> Option<String> {
        let blob = self
            .with_shared(|inner| self.raw_blob_mut(inner, key.as_bytes()))
            .ok()
            .flatten()?;
        mini_pb::decode_str(&blob).ok().map(str::to_owned)
    }

    pub fn get_str_or(&self, key: &str, default: &str) -> String {
        self.get_str(key).unwrap_or_else(|| default.to_owned())
    }

    /// C++ `getString(key, result, inplaceModification=true)` equivalent.
    pub fn get_str_into(&self, key: &str, result: &mut String) -> bool {
        let Some(blob) = self
            .with_shared(|inner| self.raw_blob_mut(inner, key.as_bytes()))
            .ok()
            .flatten()
        else {
            return false;
        };
        match mini_pb::decode_str(&blob) {
            Ok(value) => {
                result.clear();
                result.push_str(value);
                true
            }
            Err(_) => false,
        }
    }

    /// Scoped read of a byte value. The closure runs while the store lock is
    /// held, avoiding an extra caller-side copy of the decoded value.
    pub fn with_bytes<R>(&self, key: &str, f: impl FnOnce(Option<&[u8]>) -> R) -> R {
        match self.with_shared(|inner| self.raw_blob_mut(inner, key.as_bytes())) {
            Ok(Some(blob)) => match mini_pb::decode_bytes(&blob) {
                Ok(value) => f(Some(value)),
                Err(_) => f(None),
            },
            _ => f(None),
        }
    }

    pub fn with_str<R>(&self, key: &str, f: impl FnOnce(Option<&str>) -> R) -> R {
        match self.with_shared(|inner| self.raw_blob_mut(inner, key.as_bytes())) {
            Ok(Some(blob)) => match mini_pb::decode_str(&blob) {
                Ok(value) => f(Some(value)),
                Err(_) => f(None),
            },
            _ => f(None),
        }
    }

    pub fn get_bytes_or(&self, key: &str, default: &[u8]) -> Vec<u8> {
        self.get_bytes(key).unwrap_or_else(|| default.to_vec())
    }

    pub fn get_bytes(&self, key: &str) -> Option<Vec<u8>> {
        let blob = self
            .with_shared(|inner| self.raw_blob_mut(inner, key.as_bytes()))
            .ok()
            .flatten()?;
        mini_pb::decode_bytes(&blob).ok().map(<[u8]>::to_vec)
    }

    pub fn get_strings_or(&self, key: &str, default: &[String]) -> Vec<String> {
        self.get_strings(key).unwrap_or_else(|| default.to_vec())
    }

    pub fn get_strings(&self, key: &str) -> Option<Vec<String>> {
        let blob = self
            .with_shared(|inner| self.raw_blob_mut(inner, key.as_bytes()))
            .ok()
            .flatten()?;
        mini_pb::decode_strings(&blob).ok()
    }

    pub fn get_bool_vec(&self, key: &str) -> Option<Vec<bool>> {
        let blob = self
            .with_shared(|inner| self.raw_blob_mut(inner, key.as_bytes()))
            .ok()
            .flatten()?;
        mini_pb::decode_bool_vec(&blob).ok()
    }

    pub fn get_i32_vec(&self, key: &str) -> Option<Vec<i32>> {
        let blob = self
            .with_shared(|inner| self.raw_blob_mut(inner, key.as_bytes()))
            .ok()
            .flatten()?;
        mini_pb::decode_i32_vec(&blob).ok()
    }

    pub fn get_u32_vec(&self, key: &str) -> Option<Vec<u32>> {
        let blob = self
            .with_shared(|inner| self.raw_blob_mut(inner, key.as_bytes()))
            .ok()
            .flatten()?;
        mini_pb::decode_u32_vec(&blob).ok()
    }

    pub fn get_i64_vec(&self, key: &str) -> Option<Vec<i64>> {
        let blob = self
            .with_shared(|inner| self.raw_blob_mut(inner, key.as_bytes()))
            .ok()
            .flatten()?;
        mini_pb::decode_i64_vec(&blob).ok()
    }

    pub fn get_u64_vec(&self, key: &str) -> Option<Vec<u64>> {
        let blob = self
            .with_shared(|inner| self.raw_blob_mut(inner, key.as_bytes()))
            .ok()
            .flatten()?;
        mini_pb::decode_u64_vec(&blob).ok()
    }

    pub fn get_f32_vec(&self, key: &str) -> Option<Vec<f32>> {
        let blob = self
            .with_shared(|inner| self.raw_blob_mut(inner, key.as_bytes()))
            .ok()
            .flatten()?;
        mini_pb::decode_f32_vec(&blob).ok()
    }

    pub fn get_f64_vec(&self, key: &str) -> Option<Vec<f64>> {
        let blob = self
            .with_shared(|inner| self.raw_blob_mut(inner, key.as_bytes()))
            .ok()
            .flatten()?;
        mini_pb::decode_f64_vec(&blob).ok()
    }

    pub fn contains(&self, key: &str) -> bool {
        self.with_shared(|inner| {
            if inner.crypter.is_some() {
                inner.dict_crypt.contains_key(key.as_bytes())
            } else {
                inner.dict.contains_key(key.as_bytes())
            }
        })
        .unwrap_or(false)
    }

    pub fn all_keys(&self) -> Vec<String> {
        self.with_shared(|inner| {
            if inner.crypter.is_some() {
                inner
                    .dict_crypt
                    .keys()
                    .map(|key| String::from_utf8_lossy(key).into_owned())
                    .collect()
            } else {
                inner
                    .dict
                    .keys()
                    .map(|key| String::from_utf8_lossy(key).into_owned())
                    .collect()
            }
        })
        .unwrap_or_default()
    }

    /// Keys that are not expired. Unlike MMKV's writable `filterExpire` paths
    /// this does not rewrite the store.
    pub fn all_keys_non_expired(&self) -> Vec<String> {
        self.with_shared(|inner| {
            let keys: Vec<Box<[u8]>> = if inner.crypter.is_some() {
                inner.dict_crypt.keys().cloned().collect()
            } else {
                inner.dict.keys().cloned().collect()
            };
            keys.into_iter()
                .filter(|key| !self.is_expired_key(inner, key))
                .map(|key| String::from_utf8_lossy(&key).into_owned())
                .collect()
        })
        .unwrap_or_default()
    }

    pub fn count(&self) -> usize {
        self.with_shared(|inner| Self::dictionary_len(inner))
            .unwrap_or(0)
    }

    /// Iterate keys; return `false` from the callback to stop (MMKV's block
    /// `stop` flag).
    pub fn enumerate_keys(&self, mut visit: impl FnMut(&str) -> bool) {
        for key in self.all_keys() {
            if !visit(&key) {
                break;
            }
        }
    }

    pub fn enumerate_keys_non_expired(&self, mut visit: impl FnMut(&str) -> bool) {
        for key in self.all_keys_non_expired() {
            if !visit(&key) {
                break;
            }
        }
    }

    pub fn count_non_expired(&self) -> usize {
        self.with_shared(|inner| {
            let keys: Vec<Box<[u8]>> = if inner.crypter.is_some() {
                inner.dict_crypt.keys().cloned().collect()
            } else {
                inner.dict.keys().cloned().collect()
            };
            keys.into_iter()
                .filter(|key| !self.is_expired_key(inner, key))
                .count()
        })
        .unwrap_or(0)
    }

    fn is_expired_key(&self, inner: &Inner, key: &[u8]) -> bool {
        if !inner.expiration_enabled {
            return false;
        }
        let Some(value) = self.value_blob_raw(inner, key) else {
            return false;
        };
        if value.len() < FIXED32_SIZE {
            return false;
        }
        let time = u32::from_le_bytes(
            value[value.len() - FIXED32_SIZE..]
                .try_into()
                .expect("length checked"),
        );
        time != 0 && time <= self.platform.now_unix_seconds()
    }

    pub fn total_size(&self) -> usize {
        self.with_shared(|inner| inner.data.as_ref().map(MemoryFile::size).unwrap_or(0))
            .unwrap_or(0)
    }

    pub fn actual_size(&self) -> usize {
        self.with_shared(|inner| inner.actual_size).unwrap_or(0)
    }

    pub fn value_size(&self, key: &str, actual: bool) -> usize {
        let Ok(Some(blob)) = self.with_shared(|inner| self.raw_blob_mut(inner, key.as_bytes()))
        else {
            return 0;
        };
        if actual {
            let mut input = CodedInput::new(&blob);
            if let Ok(length) = input.read_u32()
                && varint_size_u32(length) + length as usize == blob.len()
            {
                return length as usize;
            }
        }
        blob.len()
    }

    pub fn write_value(&self, key: &str, out: &mut [u8]) -> Result<usize> {
        let Some(blob) = self.with_shared(|inner| self.raw_blob_mut(inner, key.as_bytes()))? else {
            return Err(Error::InvalidId(key.to_owned()));
        };
        let mut input = CodedInput::new(&blob);
        if let Ok(length) = input.read_i32() {
            let prefix = varint_size_i32(length);
            if length >= 0 {
                let length = length as usize;
                if prefix + length == blob.len() {
                    if length <= out.len() {
                        out[..length].copy_from_slice(&blob[prefix..prefix + length]);
                        return Ok(length);
                    }
                } else if blob.len() <= out.len() {
                    out[..blob.len()].copy_from_slice(&blob);
                    return Ok(blob.len());
                }
            }
        }
        Err(Error::Corrupted(CorruptKind::Length))
    }

    // ── public writes ───────────────────────────────────────────────────

    pub fn put_bool(&self, key: &str, value: bool) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_bool(value), false, None)
    }

    pub fn put_bool_expiring(&self, key: &str, value: bool, seconds: u32) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_bool(value), false, Some(seconds))
    }

    pub fn put_i32(&self, key: &str, value: i32) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_i32(value), false, None)
    }

    pub fn put_i32_expiring(&self, key: &str, value: i32, seconds: u32) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_i32(value), false, Some(seconds))
    }

    pub fn put_u32(&self, key: &str, value: u32) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_u32(value), false, None)
    }

    pub fn put_u32_expiring(&self, key: &str, value: u32, seconds: u32) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_u32(value), false, Some(seconds))
    }

    pub fn put_i64(&self, key: &str, value: i64) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_i64(value), false, None)
    }

    pub fn put_i64_expiring(&self, key: &str, value: i64, seconds: u32) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_i64(value), false, Some(seconds))
    }

    pub fn put_u64(&self, key: &str, value: u64) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_u64(value), false, None)
    }

    pub fn put_u64_expiring(&self, key: &str, value: u64, seconds: u32) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_u64(value), false, Some(seconds))
    }

    pub fn put_f32(&self, key: &str, value: f32) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_f32(value), false, None)
    }

    pub fn put_f32_expiring(&self, key: &str, value: f32, seconds: u32) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_f32(value), false, Some(seconds))
    }

    pub fn put_f64(&self, key: &str, value: f64) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_f64(value), false, None)
    }

    pub fn put_f64_expiring(&self, key: &str, value: f64, seconds: u32) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_f64(value), false, Some(seconds))
    }

    pub fn put_str(&self, key: &str, value: &str) -> Result<()> {
        self.put_blob(key, value.as_bytes(), true, None)
    }

    pub fn put_str_expiring(&self, key: &str, value: &str, seconds: u32) -> Result<()> {
        self.put_blob(key, value.as_bytes(), true, Some(seconds))
    }

    pub fn put_bytes(&self, key: &str, value: &[u8]) -> Result<()> {
        self.put_blob(key, value, true, None)
    }

    pub fn put_bytes_expiring(&self, key: &str, value: &[u8], seconds: u32) -> Result<()> {
        self.put_blob(key, value, true, Some(seconds))
    }

    pub fn put_strings<S: AsRef<str>>(&self, key: &str, values: &[S]) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_strings(values), false, None)
    }

    pub fn put_strings_expiring<S: AsRef<str>>(
        &self,
        key: &str,
        values: &[S],
        seconds: u32,
    ) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_strings(values), false, Some(seconds))
    }

    pub fn put_bool_vec(&self, key: &str, values: &[bool]) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_bool_vec(values), false, None)
    }

    pub fn put_bool_vec_expiring(&self, key: &str, values: &[bool], seconds: u32) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_bool_vec(values), false, Some(seconds))
    }

    pub fn put_i32_vec(&self, key: &str, values: &[i32]) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_i32_vec(values), false, None)
    }

    pub fn put_i32_vec_expiring(&self, key: &str, values: &[i32], seconds: u32) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_i32_vec(values), false, Some(seconds))
    }

    pub fn put_u32_vec(&self, key: &str, values: &[u32]) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_u32_vec(values), false, None)
    }

    pub fn put_u32_vec_expiring(&self, key: &str, values: &[u32], seconds: u32) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_u32_vec(values), false, Some(seconds))
    }

    pub fn put_i64_vec(&self, key: &str, values: &[i64]) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_i64_vec(values), false, None)
    }

    pub fn put_i64_vec_expiring(&self, key: &str, values: &[i64], seconds: u32) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_i64_vec(values), false, Some(seconds))
    }

    pub fn put_u64_vec(&self, key: &str, values: &[u64]) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_u64_vec(values), false, None)
    }

    pub fn put_u64_vec_expiring(&self, key: &str, values: &[u64], seconds: u32) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_u64_vec(values), false, Some(seconds))
    }

    pub fn put_f32_vec(&self, key: &str, values: &[f32]) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_f32_vec(values), false, None)
    }

    pub fn put_f32_vec_expiring(&self, key: &str, values: &[f32], seconds: u32) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_f32_vec(values), false, Some(seconds))
    }

    pub fn put_f64_vec(&self, key: &str, values: &[f64]) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_f64_vec(values), false, None)
    }

    pub fn put_f64_vec_expiring(&self, key: &str, values: &[f64], seconds: u32) -> Result<()> {
        self.put_blob(key, &mini_pb::encode_f64_vec(values), false, Some(seconds))
    }

    pub fn remove(&self, key: &str) -> Result<bool> {
        if key.is_empty() {
            return Ok(false);
        }
        self.with_exclusive(|inner| self.remove_locked(inner, key.as_bytes()))
    }

    pub fn remove_many(&self, keys: &[&str]) -> Result<()> {
        for key in keys {
            let _ = self.remove(key)?;
        }
        Ok(())
    }

    pub fn clear(&self) {
        let _ = self.clear_all(false);
    }

    pub fn clear_keep_space(&self) {
        let _ = self.clear_all(true);
    }

    fn clear_all(&self, keep_space: bool) -> Result<()> {
        self.with_exclusive(|inner| self.clear_all_locked(inner, keep_space))
    }

    pub fn trim(&self) {
        let _ = self.trim_locked();
    }

    fn trim_locked(&self) -> Result<()> {
        let mut inner = self.inner.lock();
        if inner.closed {
            return Err(Error::Closed);
        }
        if inner.read_only {
            return Err(Error::ReadOnly);
        }
        let _guard = self.lock.exclusive_guard()?;
        self.ensure_loaded_locked(&mut inner)?;
        if inner.actual_size == 0 {
            return self.clear_all_locked(&mut inner, false);
        }
        let old_size = inner.data.as_ref().map(MemoryFile::size).unwrap_or(0);
        if old_size <= inner.expected_capacity {
            return Ok(());
        }
        if !inner.has_full_writeback {
            self.full_writeback_locked(&mut inner)?;
        }
        let mut file_size = old_size;
        while file_size > (inner.actual_size + FIXED32_SIZE) * 2 {
            file_size /= 2;
        }
        file_size = file_size.max(inner.expected_capacity);
        if file_size == old_size {
            return Ok(());
        }
        inner
            .data
            .as_mut()
            .expect("data exists")
            .resize(file_size)?;
        inner.output_position = inner.actual_size;
        Ok(())
    }

    pub fn clear_memory_cache(&self, keep_space: bool) {
        let mut inner = self.inner.lock();
        self.clear_memory_cache_locked(&mut inner, keep_space);
    }

    pub fn sync(&self) {
        let _ = self.sync_impl(SyncMode::Sync);
    }

    pub fn sync_async(&self) {
        let _ = self.sync_impl(SyncMode::Async);
    }

    fn sync_impl(&self, mode: SyncMode) -> Result<()> {
        let mut inner = self.inner.lock();
        if inner.closed {
            return Err(Error::Closed);
        }
        let _guard = if inner.read_only {
            self.lock.shared_guard()?
        } else {
            self.lock.exclusive_guard()?
        };
        self.ensure_loaded_locked(&mut inner)?;
        self.sync_locked(&mut inner, mode)
    }

    pub fn check_content_changed(&self) {
        let mut inner = self.inner.lock();
        let _ = self.ensure_loaded_locked(&mut inner);
    }

    pub fn lock(&self) -> Result<StoreLockGuard<'_>> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        let guard = self.lock.exclusive_guard()?;
        Ok(StoreLockGuard { _guard: guard })
    }

    pub fn try_lock(&self) -> Option<StoreLockGuard<'_>> {
        if self.is_closed() {
            return None;
        }
        match self.lock.try_exclusive_guard() {
            Ok(Some(guard)) => Some(StoreLockGuard { _guard: guard }),
            _ => None,
        }
    }

    // ── expiration ──────────────────────────────────────────────────────

    fn configure_expiration_locked(&self, inner: &mut Inner, expiration: Expiration) -> Result<()> {
        if inner.read_only {
            inner.expired_in_seconds = expiration.seconds();
            return Ok(());
        }
        match expiration {
            Expiration::PerKey => self.enable_expiration_locked(inner, 0),
            Expiration::After(seconds) => self.enable_expiration_locked(inner, seconds),
        }
    }

    fn enable_expiration_locked(&self, inner: &mut Inner, seconds: u32) -> Result<()> {
        inner.expired_in_seconds = seconds;
        if inner.expiration_enabled {
            inner.compare_before_set = false;
            return Ok(());
        }
        if inner.read_only {
            return Err(Error::ReadOnly);
        }
        if inner.meta_info.has_flag(meta::FLAG_ENABLE_KEY_EXPIRE) {
            inner.expiration_enabled = true;
            inner.compare_before_set = false;
            return Ok(());
        }

        let time = if seconds != 0 {
            self.platform.now_unix_seconds().saturating_add(seconds)
        } else {
            0
        };
        let empty = inner.actual_size == 0 && Self::dictionary_is_empty(inner);

        inner.expiration_enabled = true;
        inner.compare_before_set = false;
        inner.meta_info.set_flag(meta::FLAG_ENABLE_KEY_EXPIRE);
        inner.meta_info.version = meta::VERSION_FLAG;

        if empty {
            self.write_actual_size_locked(inner, 0, 0, true, None)?;
            inner.meta.sync(SyncMode::Sync)?;
            return Ok(());
        }

        let entries = self
            .all_values_raw_locked(inner)?
            .into_iter()
            .map(|(key, mut value)| {
                value.extend_from_slice(&time.to_le_bytes());
                (key, value)
            })
            .collect();
        self.write_all_entries_locked(inner, entries, true)
    }

    fn disable_expiration_locked(&self, inner: &mut Inner) -> Result<()> {
        if !inner.expiration_enabled {
            return Ok(());
        }
        if inner.read_only {
            return Err(Error::ReadOnly);
        }
        let entries = self
            .all_values_raw_locked(inner)?
            .into_iter()
            .map(|(key, mut value)| {
                if value.len() >= FIXED32_SIZE {
                    value.truncate(value.len() - FIXED32_SIZE);
                }
                (key, value)
            })
            .collect();
        inner.expiration_enabled = false;
        inner.expired_in_seconds = 0;
        inner.meta_info.unset_flag(meta::FLAG_ENABLE_KEY_EXPIRE);
        inner.meta_info.version = meta::VERSION_FLAG;
        self.write_all_entries_locked(inner, entries, true)
    }

    fn all_values_raw_locked(&self, inner: &mut Inner) -> Result<RawEntries> {
        let keys: Vec<Box<[u8]>> = if inner.crypter.is_some() {
            inner.dict_crypt.keys().cloned().collect()
        } else {
            inner.dict.keys().cloned().collect()
        };
        let mut entries = Vec::with_capacity(keys.len());
        for key in keys {
            let value = self
                .value_blob_raw(inner, &key)
                .ok_or(Error::Corrupted(CorruptKind::Length))?;
            entries.push((key, value));
        }
        Ok(entries)
    }

    fn filter_expired_locked(&self, inner: &mut Inner) -> usize {
        if !inner.expiration_enabled {
            return 0;
        }
        let keys: Vec<Box<[u8]>> = if inner.crypter.is_some() {
            inner.dict_crypt.keys().cloned().collect()
        } else {
            inner.dict.keys().cloned().collect()
        };
        let expired: Vec<Box<[u8]>> = keys
            .into_iter()
            .filter(|key| self.is_expired_key(inner, key))
            .collect();
        for key in &expired {
            if inner.crypter.is_some() {
                inner.dict_crypt.remove(key);
            } else {
                inner.dict.remove(key);
            }
        }
        expired.len()
    }

    pub fn enable_expiration(&self, seconds: u32) -> Result<()> {
        self.with_exclusive(|inner| self.enable_expiration_locked(inner, seconds))
    }

    pub fn disable_expiration(&self) -> Result<()> {
        self.with_exclusive(|inner| self.disable_expiration_locked(inner))
    }

    pub fn enable_compare_before_set(&self) -> Result<()> {
        self.with_exclusive(|inner| {
            if inner.expiration_enabled {
                return Err(Error::Unsupported(
                    "compare-before-set is invalid when expiration is enabled",
                ));
            }
            if inner.crypter.is_some() {
                return Err(Error::Unsupported(
                    "compare-before-set is invalid when encryption is enabled",
                ));
            }
            inner.compare_before_set = true;
            Ok(())
        })
    }

    pub fn disable_compare_before_set(&self) -> Result<()> {
        self.with_exclusive(|inner| {
            inner.compare_before_set = false;
            Ok(())
        })
    }

    // ── import ──────────────────────────────────────────────────────────

    pub fn import_from(&self, source: &Store) -> Result<usize> {
        if std::ptr::eq(self, source) {
            return Ok(0);
        }

        // Lock both stores in a stable order (path, then pointer) to avoid
        // A <- B / B <- A deadlocks across threads and processes.
        let self_key = (self.path.clone(), self as *const Store as usize);
        let source_key = (source.path.clone(), source as *const Store as usize);
        let (first, second) = if self_key <= source_key {
            (self, source)
        } else {
            (source, self)
        };

        let mut first_inner = first.inner.lock();
        let mut second_inner = second.inner.lock();
        let _first_guard = if first.mode.is_read_only() {
            first.lock.shared_guard()?
        } else {
            first.lock.exclusive_guard()?
        };
        let _second_guard = if second.mode.is_read_only() {
            second.lock.shared_guard()?
        } else {
            second.lock.exclusive_guard()?
        };
        first.ensure_loaded_locked(&mut first_inner)?;
        second.ensure_loaded_locked(&mut second_inner)?;

        let (dest, dest_inner, src, src_inner) = if std::ptr::eq(first, self) {
            (self, &mut *first_inner, source, &mut *second_inner)
        } else {
            (self, &mut *second_inner, source, &mut *first_inner)
        };

        let keys: Vec<Box<[u8]>> = if src_inner.crypter.is_some() {
            src_inner.dict_crypt.keys().cloned().collect()
        } else {
            src_inner.dict.keys().cloned().collect()
        };

        let mut count = 0usize;
        for key in keys {
            let Some(blob) = src.raw_blob_mut(src_inner, &key) else {
                continue;
            };
            if blob.is_empty() {
                continue;
            }
            let expire = if dest_inner.expiration_enabled {
                Some(dest_inner.expired_in_seconds)
            } else {
                None
            };
            if dest
                .put_blob_locked(dest_inner, &key, &blob, false, expire)
                .is_ok()
            {
                count += 1;
            }
        }
        Ok(count)
    }
}

pub struct StoreLockGuard<'a> {
    _guard: ProcessGuard<'a>,
}

// ── free helpers ────────────────────────────────────────────────────────

fn recover_unreadable(path: &Path) {
    if !path.exists() {
        return;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return;
    };
    let mut probe = [0u8; 4];
    match std::io::Read::read_exact(&mut file, &mut probe) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::NotFound
                    | std::io::ErrorKind::PermissionDenied
            ) => {}
        Err(error) => {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            let mut name = path.as_os_str().to_os_string();
            name.push(format!(".corrupt.{stamp}"));
            let quarantine = PathBuf::from(name);
            mmkv_log!(
                crate::handler::Level::Warning,
                "MMKV file [{}] is unreadable ({}), moving to {}",
                path.display(),
                error,
                quarantine.display()
            );
            let _ = std::fs::rename(path, quarantine);
        }
    }
}

fn strategy_for_crc(config: Option<Recover>, id: &str) -> Recover {
    config.unwrap_or_else(|| {
        handler()
            .map(|handler| handler.on_crc_check_fail(id))
            .unwrap_or(Recover::Discard)
    })
}

fn strategy_for_length(config: Option<Recover>, id: &str) -> Recover {
    config.unwrap_or_else(|| {
        handler()
            .map(|handler| handler.on_file_length_error(id))
            .unwrap_or(Recover::Discard)
    })
}

fn last_confirmed(data: &[u8], file_size: usize, meta: &MetaInfo) -> Option<usize> {
    if meta.version < meta::VERSION_ACTUAL_SIZE {
        return None;
    }
    let last = meta.last_actual_size as usize;
    if last < file_size
        && last + FIXED32_SIZE <= file_size
        && crc32(0, &data[FIXED32_SIZE..FIXED32_SIZE + last]) == meta.last_crc_digest
    {
        Some(last)
    } else {
        None
    }
}

fn encoded_value_length(blob: &[u8], data_holder: bool) -> Result<u32> {
    let mut length = blob.len() as u64;
    if data_holder {
        length += varint_size_u32(blob.len() as u32) as u64;
    }
    if length > u32::MAX as u64 {
        return Err(Error::TooLarge {
            key: "<value>".to_owned(),
            size: length.min(usize::MAX as u64) as usize,
        });
    }
    Ok(length as u32)
}

fn encoded_entry(key: &[u8], blob: &[u8], data_holder: bool) -> Result<(u32, usize)> {
    if key.len() > KEY_SIZE_LIMIT as usize || key.len() > u32::MAX as usize {
        return Err(Error::TooLarge {
            key: String::from_utf8_lossy(key).into_owned(),
            size: key.len(),
        });
    }
    if blob.len() > u32::MAX as usize {
        return Err(Error::TooLarge {
            key: String::from_utf8_lossy(key).into_owned(),
            size: blob.len(),
        });
    }
    let value_len = encoded_value_length(blob, data_holder)?;
    let total = varint_size_u32(key.len() as u32)
        + key.len()
        + varint_size_u32(value_len)
        + value_len as usize;
    Ok((value_len, total))
}

fn write_entry_at(
    buffer: &mut [u8],
    start: usize,
    key: &[u8],
    blob: &[u8],
    data_holder: bool,
) -> Result<usize> {
    let (value_len, total) = encoded_entry(key, blob, data_holder)?;
    if start + total > buffer.len() {
        return Err(Error::Corrupted(CorruptKind::Length));
    }
    let mut output = CodedOutput::new(&mut buffer[start..start + total]);
    output.write_varint32(key.len() as u32)?;
    output.write_raw(key)?;
    output.write_varint32(value_len)?;
    if data_holder {
        output.write_varint32(blob.len() as u32)?;
    }
    output.write_raw(blob)?;
    debug_assert_eq!(output.position(), total);
    Ok(total)
}

fn equal_encoded(old: &[u8], blob: &[u8], data_holder: bool) -> bool {
    if data_holder {
        let mut expected = Vec::with_capacity(blob.len() + 5);
        crate::codec::varint::append_varint32(&mut expected, blob.len() as u32);
        expected.extend_from_slice(blob);
        old == expected.as_slice()
    } else {
        old == blob
    }
}

fn decode_entries(payload: &[u8], position: usize, greedy: bool) -> Result<Vec<Entry>> {
    let mut input = CodedInput::new(payload);
    let mut entries = Vec::new();

    let run = |input: &mut CodedInput<'_>, entries: &mut Vec<Entry>| -> Result<()> {
        if position == 0 {
            input.read_u32()?;
        } else {
            input.seek(position)?;
        }
        while !input.is_at_end() {
            let record_start = input.position();
            let key_len = input.read_u32()?;
            if key_len > KEY_SIZE_LIMIT {
                return Err(Error::Corrupted(CorruptKind::Length));
            }
            let key = input
                .read_bytes(key_len as usize)?
                .to_vec()
                .into_boxed_slice();
            let value_len = input.read_u32()?;
            let computed = input.position() - record_start;
            if computed > u16::MAX as usize {
                return Err(Error::Corrupted(CorruptKind::Length));
            }
            input.skip(value_len as usize)?;
            let holder = KeyValueHolder {
                computed_kv_size: computed as u16,
                key_size: key_len as u16,
                value_size: value_len,
                offset: record_start as u32,
            };
            if value_len == 0 {
                entries.push((key, None));
            } else {
                entries.push((key, Some(holder)));
            }
        }
        Ok(())
    };

    match run(&mut input, &mut entries) {
        Ok(()) => Ok(entries),
        Err(err) => {
            if greedy {
                Ok(entries)
            } else {
                Err(err)
            }
        }
    }
}

fn apply_entries(dict: &mut Dictionary, entries: Vec<Entry>) {
    for (key, holder) in entries {
        match holder {
            Some(holder) => {
                dict.insert(key, holder);
            }
            None => {
                dict.remove(key.as_ref());
            }
        }
    }
}

/// Decrypt `payload[position..]` in one CFB pass, then parse the plaintext
/// records. Large values stay encrypted on disk and are represented by an
/// offset holder with the CFB status captured at the record start.
fn decode_entries_crypt(
    payload: &[u8],
    position: usize,
    greedy: bool,
    crypter: &AesCrypt,
) -> Result<Vec<CryptEntry>> {
    let Some(mut reader) = CryptReader::new(payload, position, crypter) else {
        return Err(Error::Corrupted(CorruptKind::Length));
    };
    if position == 0 && reader.read_u32().is_none() {
        return Err(Error::Corrupted(CorruptKind::Length));
    }

    let mut entries = Vec::new();
    let mut run = || -> Result<()> {
        while !reader.is_at_end() {
            let record_start = reader.position();
            let key_len = reader
                .read_u32()
                .ok_or(Error::Corrupted(CorruptKind::Length))?;
            if key_len > KEY_SIZE_LIMIT {
                return Err(Error::Corrupted(CorruptKind::Length));
            }
            let key = reader
                .read_bytes(key_len as usize)
                .ok_or(Error::Corrupted(CorruptKind::Length))?
                .into_boxed_slice();
            let value_len = reader
                .read_u32()
                .ok_or(Error::Corrupted(CorruptKind::Length))?;
            if value_len == 0 {
                entries.push((key, None));
            } else if KeyValueHolderCrypt::is_value_stored_as_offset(value_len as usize) {
                if !reader.skip(value_len as usize) {
                    return Err(Error::Corrupted(CorruptKind::Length));
                }
                let status = crypter.status_at(payload, record_start);
                let holder =
                    KeyValueHolderCrypt::offset(key_len, value_len, record_start as u32, status)
                        .ok_or(Error::Corrupted(CorruptKind::Length))?;
                entries.push((key, Some(holder)));
            } else {
                let value = reader
                    .read_bytes(value_len as usize)
                    .ok_or(Error::Corrupted(CorruptKind::Length))?;
                entries.push((key, Some(KeyValueHolderCrypt::from_value(&value))));
            }
        }
        Ok(())
    };

    match run() {
        Ok(()) => Ok(entries),
        Err(err) => {
            if greedy {
                Ok(entries)
            } else {
                Err(err)
            }
        }
    }
}

fn apply_crypt_entries(dict: &mut CryptDictionary, entries: Vec<CryptEntry>) {
    for (key, holder) in entries {
        match holder {
            Some(holder) => {
                dict.insert(key, holder);
            }
            None => {
                dict.remove(key.as_ref());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_sizes_match_cpp_layout() {
        let (value_len, total) = encoded_entry(b"abc", b"hello", false).unwrap();
        assert_eq!(value_len, 5);
        assert_eq!(total, 1 + 3 + 1 + 5);

        let (value_len, total) = encoded_entry(b"abc", b"hello", true).unwrap();
        assert_eq!(value_len, 6);
        assert_eq!(total, 1 + 3 + 1 + 6);
    }

    #[test]
    fn decode_accepts_duplicates_and_tombstones() {
        let mut buffer = Vec::new();
        crate::codec::varint::append_varint32(&mut buffer, FAKE_MAP_SIZE);
        for (value, data_holder) in [
            (Some(b"one".as_slice()), false),
            (Some(b"two".as_slice()), false),
            (None, false),
        ] {
            crate::codec::varint::append_varint32(&mut buffer, 3);
            buffer.extend_from_slice(b"key");
            match value {
                Some(value) => {
                    let _ = data_holder;
                    crate::codec::varint::append_varint32(&mut buffer, value.len() as u32);
                    buffer.extend_from_slice(value);
                }
                None => crate::codec::varint::append_varint32(&mut buffer, 0),
            }
        }
        let entries = decode_entries(&buffer, 0, false).unwrap();
        assert_eq!(entries.len(), 3);
        assert!(entries[2].1.is_none());
    }
}
