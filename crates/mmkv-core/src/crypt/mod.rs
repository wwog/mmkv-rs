//! AES-CFB128 layer, bit-compatible with MMKV v2.4.3.

pub mod aes;
pub mod holder;

pub use aes::{AES_IV_LEN, AES_KEY_LEN, AES256_KEY_LEN, AesCrypt, AesStatus};
pub use holder::{KeyValueHolderCrypt, MEDIUM_BUFFER_SIZE, SMALL_BUFFER_SIZE};
