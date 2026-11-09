//! Store file naming, matching `MMKV.cpp`'s `encodeFilePath`,
//! `mappedKVPathWithID`, `crcPathWithPath` and `mmapedKVKey`.

use std::path::{Path, PathBuf};

use md5::{Digest, Md5};

pub const DEFAULT_ID: &str = "mmkv.default";
pub const CRC_SUFFIX: &str = ".crc";
pub const SPECIAL_CHARACTER_DIRECTORY_NAME: &str = "specialCharacter";

const SPECIAL_CHARACTERS: &[char] = &['\\', '/', ':', '*', '?', '"', '<', '>', '|'];

pub fn md5_hex(data: &[u8]) -> String {
    let digest = Md5::digest(data);
    let mut out = String::with_capacity(32);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

pub fn has_special_characters(id: &str) -> bool {
    id.chars().any(|ch| SPECIAL_CHARACTERS.contains(&ch))
}

/// Relative file name for a store id. Special characters are replaced by
/// `specialCharacter/<md5(id)>`.
pub fn encode_file_path(id: &str) -> String {
    if has_special_characters(id) {
        format!(
            "{SPECIAL_CHARACTER_DIRECTORY_NAME}/{}",
            md5_hex(id.as_bytes())
        )
    } else {
        id.to_owned()
    }
}

pub fn mapped_kv_path(root: &Path, id: &str) -> PathBuf {
    root.join(encode_file_path(id))
}

pub fn crc_path(kv_path: &Path) -> PathBuf {
    let mut path = kv_path.as_os_str().to_os_string();
    path.push(CRC_SUFFIX);
    PathBuf::from(path)
}

/// In-memory instance-cache key (`mmapedKVKey`): md5 of `root + "/" + id`.
pub fn registry_key(root: &Path, id: &str) -> String {
    md5_hex(format!("{}/{}", root.to_string_lossy(), id).as_bytes())
}

/// Canonicalise a path the way `absolutePath()` does: absolute first, then
/// resolve existing components (`weakly_canonical`).
pub fn absolute_path(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    match absolute.canonicalize() {
        Ok(canonical) => canonical,
        Err(_) => absolute,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_ids_are_used_verbatim() {
        assert_eq!(encode_file_path("mmkv.default"), "mmkv.default");
        assert_eq!(encode_file_path("user-42_session"), "user-42_session");
        assert!(!has_special_characters("abc.def"));
    }

    #[test]
    fn special_ids_use_md5_and_subdirectory() {
        let encoded = encode_file_path("a/b");
        assert_eq!(encoded, "specialCharacter/a7e86136543b019d72468ceebf71fb8e");
    }

    #[test]
    fn crc_suffix_is_appended() {
        assert_eq!(
            crc_path(Path::new("/tmp/mmkv.default")),
            PathBuf::from("/tmp/mmkv.default.crc")
        );
    }
}
