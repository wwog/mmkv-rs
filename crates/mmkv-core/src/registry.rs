//! Global initialization and instance registry (`g_instanceDic`).
//!
//! MMKV caches instances by `md5(root + "/" + id)`; reopening the same id and
//! root returns the same handle. We keep `Weak` references so dropping all
//! handles lets the store close naturally.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

use mmkv_platform::{AppEvent, Platform};
use parking_lot::Mutex;

use crate::config::StoreConfig;
use crate::error::{Error, Result};
use crate::handler::{Handler, Level};
use crate::log;
use crate::path::{DEFAULT_ID, absolute_path, registry_key};
use crate::store::{Mmkv, Store};

struct Global {
    platform: Arc<dyn Platform>,
    root: PathBuf,
    instances: Mutex<HashMap<String, Weak<Store>>>,
}

static GLOBAL: OnceLock<Global> = OnceLock::new();

pub fn is_initialized() -> bool {
    GLOBAL.get().is_some()
}

pub fn init(
    platform: Arc<dyn Platform>,
    root: Option<PathBuf>,
    log_level: Level,
    handler: Option<Arc<dyn Handler>>,
) -> Result<PathBuf> {
    log::set_level(log_level);
    log::set_handler(handler);

    if let Some(global) = GLOBAL.get() {
        return Ok(global.root.clone());
    }

    let root = match root {
        Some(root) => absolute_path(&root),
        None => absolute_path(&platform.default_root_dir()?),
    };
    platform.create_dir_all(&root)?;

    // Memory warnings drop cached key/value dictionaries without unmapping the
    // metadata file (MMKV does the same on iOS).
    let _cancel = platform.subscribe_app_events(Box::new(|event| {
        if event != AppEvent::MemoryWarning {
            return;
        }
        let Some(global) = GLOBAL.get() else {
            return;
        };
        let stores: Vec<Mmkv> = {
            let map = global.instances.lock();
            map.values().filter_map(Weak::upgrade).collect()
        };
        for store in stores {
            store.clear_memory_cache(false);
        }
    }));

    let global = Global {
        platform,
        root,
        instances: Mutex::new(HashMap::new()),
    };
    let root = global.root.clone();
    let _ = GLOBAL.set(global);
    Ok(root)
}

pub fn root_dir() -> Option<PathBuf> {
    GLOBAL.get().map(|global| global.root.clone())
}

pub fn platform() -> Option<Arc<dyn Platform>> {
    GLOBAL.get().map(|global| global.platform.clone())
}

pub fn default_root_dir() -> Result<PathBuf> {
    GLOBAL
        .get()
        .map(|global| global.root.clone())
        .ok_or(Error::NotInitialized)
}

pub fn default_store(config: &StoreConfig<'_>) -> Result<Mmkv> {
    open(DEFAULT_ID, config)
}

pub fn open(id: impl Into<String>, config: &StoreConfig<'_>) -> Result<Mmkv> {
    let global = GLOBAL.get().ok_or(Error::NotInitialized)?;
    let root = match config.root {
        Some(root) => absolute_path(root),
        None => global.root.clone(),
    };
    open_registered(global.platform.clone(), &root, &id.into(), config)
}

/// Open a store under an explicit root, sharing the global instance cache when
/// one exists. Used by `Namespace` and by integration tests.
pub fn open_at(
    platform: Arc<dyn Platform>,
    root: &Path,
    id: &str,
    config: &StoreConfig<'_>,
) -> Result<Mmkv> {
    open_registered(platform, &absolute_path(root), id, config)
}

fn open_registered(
    platform: Arc<dyn Platform>,
    root: &Path,
    id: &str,
    config: &StoreConfig<'_>,
) -> Result<Mmkv> {
    let key = registry_key(root, id);
    if let Some(global) = GLOBAL.get() {
        let mut map = global.instances.lock();
        if let Some(existing) = map.get(&key).and_then(Weak::upgrade) {
            if existing.is_closed() {
                map.remove(&key);
            } else {
                return Ok(existing);
            }
        }
    }

    let store = Store::open_at(platform, root, id, config)?;
    if let Some(global) = GLOBAL.get() {
        global.instances.lock().insert(key, Arc::downgrade(&store));
    }
    Ok(store)
}

/// Remove an instance from the cache (called by `Mmkv::close`).
pub fn forget(store: &Store) {
    let Some(global) = GLOBAL.get() else {
        return;
    };
    let key = registry_key(store.root(), store.id());
    let mut map = global.instances.lock();
    if let Some(existing) = map.get(&key)
        && existing
            .upgrade()
            .map(|existing| std::ptr::eq(existing.as_ref(), store))
            .unwrap_or(false)
    {
        map.remove(&key);
    }
}

/// Close a cached instance by id/root (used by `removeStorage`).
pub fn close_instance(id: &str, root: &Path) {
    let Some(global) = GLOBAL.get() else {
        return;
    };
    let key = registry_key(root, id);
    let store = {
        let mut map = global.instances.lock();
        map.remove(&key).and_then(|weak| weak.upgrade())
    };
    if let Some(store) = store {
        store.close();
    }
}

/// Flush and close every live instance.
pub fn shutdown() {
    let Some(global) = GLOBAL.get() else {
        return;
    };
    let stores: Vec<Mmkv> = {
        let mut map = global.instances.lock();
        let stores = map.values().filter_map(Weak::upgrade).collect();
        map.clear();
        stores
    };
    for store in stores {
        store.sync();
        store.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::NoopHandler;

    #[test]
    fn init_returns_the_root_once() {
        let dir = tempfile::tempdir().unwrap();
        let platform = Arc::new(mmkv_platform_posix::PosixPlatform::new(Some(
            dir.path().to_path_buf(),
        )));
        let root = init(platform, None, Level::Error, Some(Arc::new(NoopHandler))).unwrap();
        assert_eq!(root, absolute_path(dir.path()));
    }
}
