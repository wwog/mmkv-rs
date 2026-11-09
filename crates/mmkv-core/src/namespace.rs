//! `NameSpace` equivalent: a store factory bound to a custom root.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mmkv_platform::Platform;

use crate::config::StoreConfig;
use crate::error::Result;
use crate::path::absolute_path;
use crate::registry;
use crate::store::{Mmkv, Store};

pub struct Namespace {
    platform: Arc<dyn Platform>,
    root: PathBuf,
}

impl Namespace {
    pub fn new(platform: Arc<dyn Platform>, root: impl AsRef<Path>) -> Self {
        Self {
            platform,
            root: absolute_path(root.as_ref()),
        }
    }

    pub fn root_dir(&self) -> &Path {
        &self.root
    }

    pub fn open(&self, id: impl Into<String>, config: &StoreConfig<'_>) -> Result<Mmkv> {
        registry::open_at(self.platform.clone(), &self.root, &id.into(), config)
    }

    pub fn default_store(&self, config: &StoreConfig<'_>) -> Result<Mmkv> {
        self.open(crate::path::DEFAULT_ID, config)
    }

    pub fn is_file_valid(&self, id: &str) -> bool {
        Store::is_file_valid(id, &self.root)
    }

    pub fn exists(&self, id: &str) -> bool {
        Store::exists(id, &self.root)
    }

    pub fn remove_storage(&self, id: &str) -> Result<bool> {
        Store::remove_storage(id, &self.root, self.platform.as_ref())
    }

    pub fn backup_one(&self, id: &str, dst_dir: &Path) -> bool {
        Store::backup_one(id, dst_dir, &self.root, self.platform.as_ref())
    }

    pub fn restore_one(&self, id: &str, src_dir: &Path) -> bool {
        Store::restore_one(id, src_dir, &self.root, self.platform.as_ref())
    }

    pub fn backup_all(&self, dst_dir: &Path) -> usize {
        Store::backup_all(dst_dir, &self.root, self.platform.as_ref())
    }

    pub fn restore_all(&self, src_dir: &Path) -> usize {
        Store::restore_all(src_dir, &self.root, self.platform.as_ref())
    }
}

impl std::fmt::Debug for Namespace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Namespace")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}
