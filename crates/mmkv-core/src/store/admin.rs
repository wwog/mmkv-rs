//! Static store administration: validity checks, removal, backup/restore.
//!
//! These mirror `MMKV::isFileValid`, `checkExist`, `removeStorage`,
//! `backupOneToDirectory` and friends.

use std::fs;
use std::io;
use std::path::Path;

use crate::crc32::crc32;
use crate::error::Result;
use crate::meta::{META_SIZE, VERSION_ACTUAL_SIZE};
use crate::path::{SPECIAL_CHARACTER_DIRECTORY_NAME, crc_path, encode_file_path, mapped_kv_path};

const FIXED32_SIZE: usize = 4;

impl super::Store {
    /// `MMKV::isFileValid`: validate the meta CRC against the payload. Returns
    /// `true` when the data file is missing (MMKV's legacy behaviour), and
    /// `false` when the `.crc` file is missing but data exists.
    pub fn is_file_valid(id: &str, root: &Path) -> bool {
        let kv_path = mapped_kv_path(root, id);
        let crc_path = crc_path(&kv_path);
        if !kv_path.exists() {
            return true;
        }
        if !crc_path.exists() {
            return false;
        }

        let Ok(crc_bytes) = fs::read(&crc_path) else {
            return false;
        };
        if crc_bytes.len() < META_SIZE {
            return false;
        }
        let Some(meta) = crate::meta::MetaInfo::read(&crc_bytes) else {
            return false;
        };
        let Ok(data) = fs::read(&kv_path) else {
            return false;
        };
        if data.len() < FIXED32_SIZE {
            return false;
        }
        let actual_size = if meta.version >= VERSION_ACTUAL_SIZE {
            meta.actual_size as usize
        } else {
            u32::from_le_bytes(data[0..4].try_into().expect("length checked")) as usize
        };
        if actual_size > data.len() - FIXED32_SIZE {
            return false;
        }
        crc32(0, &data[FIXED32_SIZE..FIXED32_SIZE + actual_size]) == meta.crc_digest
    }

    /// `checkExist`: both the data file and its `.crc` companion exist.
    pub fn exists(id: &str, root: &Path) -> bool {
        let kv_path = mapped_kv_path(root, id);
        kv_path.exists() && crc_path(&kv_path).exists()
    }

    /// `removeStorage`: unlink the data and meta files.
    pub fn remove_storage(id: &str, root: &Path) -> Result<bool> {
        crate::registry::close_instance(id, root);
        let kv_path = mapped_kv_path(root, id);
        let crc_path = crc_path(&kv_path);
        if !kv_path.exists() && !crc_path.exists() {
            return Ok(false);
        }
        if kv_path.exists() {
            fs::remove_file(&kv_path)?;
        }
        if crc_path.exists() {
            fs::remove_file(&crc_path)?;
        }
        Ok(true)
    }

    pub fn backup_one(id: &str, dst_dir: &Path, src_root: &Path) -> bool {
        let source = mapped_kv_path(src_root, id);
        let destination = dst_dir.join(encode_file_path(id));
        copy_pair(&source, &destination).is_ok()
    }

    pub fn restore_one(id: &str, src_dir: &Path, dst_root: &Path) -> bool {
        let source = src_dir.join(encode_file_path(id));
        let destination = mapped_kv_path(dst_root, id);
        copy_pair(&source, &destination).is_ok()
    }

    pub fn backup_all(dst_dir: &Path, src_root: &Path) -> usize {
        copy_tree(src_root, dst_dir)
    }

    pub fn restore_all(src_dir: &Path, dst_root: &Path) -> usize {
        copy_tree(src_dir, dst_root)
    }
}

fn copy_pair(source: &Path, destination: &Path) -> io::Result<()> {
    if !source.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("missing MMKV data file: {}", source.display()),
        ));
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, destination)?;
    let source_crc = crc_path(source);
    if source_crc.exists() {
        fs::copy(&source_crc, crc_path(destination))?;
    }
    Ok(())
}

fn copy_tree(source_dir: &Path, destination_dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(source_dir) else {
        return 0;
    };
    let mut count = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if file_type.is_dir() {
            if name == SPECIAL_CHARACTER_DIRECTORY_NAME {
                count += copy_tree(&path, &destination_dir.join(name));
            }
            continue;
        }
        if name.ends_with(".crc") {
            continue;
        }
        if copy_pair(&path, &destination_dir.join(name)).is_ok() {
            count += 1;
        }
    }
    count
}
