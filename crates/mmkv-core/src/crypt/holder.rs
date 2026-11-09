//! Encrypted value holders (`KeyValueHolderCrypt`).
//!
//! * `Direct`: plaintext cached inline, up to 27 bytes.
//! * `Memory`: plaintext cached on the heap, 28..=256 bytes.
//! * `Offset`: value stays encrypted in the mapped file; decrypted on demand
//!   using the CFB status captured at the record start.

use crate::codec::varint::varint_size_u32;
use crate::crypt::aes::{AesCrypt, AesStatus};

pub const SMALL_BUFFER_SIZE: usize = 27;
pub const MEDIUM_BUFFER_SIZE: usize = 256;

#[derive(Clone, Debug)]
pub enum KeyValueHolderCrypt {
    Direct(Vec<u8>),
    Memory(Vec<u8>),
    Offset {
        /// `varint_size(keySize) + varint_size(valueSize)`
        pb_key_value_size: u8,
        key_size: u16,
        value_size: u32,
        /// Offset of the record start in the payload (file offset 4).
        offset: u32,
        status: AesStatus,
    },
}

impl KeyValueHolderCrypt {
    pub fn from_value(value: &[u8]) -> Self {
        if value.len() <= SMALL_BUFFER_SIZE {
            Self::Direct(value.to_vec())
        } else {
            Self::Memory(value.to_vec())
        }
    }

    pub fn is_value_stored_as_offset(value_size: usize) -> bool {
        value_size > MEDIUM_BUFFER_SIZE
    }

    pub fn offset(key_size: u32, value_size: u32, offset: u32, status: AesStatus) -> Option<Self> {
        if key_size > u16::MAX as u32 {
            return None;
        }
        let pb_key_value_size = varint_size_u32(key_size) + varint_size_u32(value_size);
        if pb_key_value_size > u8::MAX as usize {
            return None;
        }
        Some(Self::Offset {
            pb_key_value_size: pb_key_value_size as u8,
            key_size: key_size as u16,
            value_size,
            offset,
            status,
        })
    }

    pub fn real_value_size(&self) -> u32 {
        match self {
            Self::Direct(value) | Self::Memory(value) => value.len() as u32,
            Self::Offset { value_size, .. } => *value_size,
        }
    }

    /// Decrypt (or return the cached) value blob. The returned bytes are the
    /// value as stored on disk, i.e. what MMKV's getters expect.
    pub fn value(&self, payload: &[u8], crypter: &AesCrypt) -> Option<Vec<u8>> {
        match self {
            Self::Direct(value) | Self::Memory(value) => Some(value.clone()),
            Self::Offset {
                pb_key_value_size,
                key_size,
                value_size,
                offset,
                status,
            } => {
                let prefix = *pb_key_value_size as usize + *key_size as usize;
                let start = *offset as usize;
                let end = start
                    .checked_add(prefix)?
                    .checked_add(*value_size as usize)?;
                let ciphertext = payload.get(start..end)?;
                let mut crypto = crypter.clone_with_status(status);
                let mut plaintext = vec![0u8; ciphertext.len()];
                crypto.decrypt_to(ciphertext, &mut plaintext);
                Some(plaintext[prefix..].to_vec())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_match_mmkv() {
        assert!(matches!(
            KeyValueHolderCrypt::from_value(&[0; 27]),
            KeyValueHolderCrypt::Direct(_)
        ));
        assert!(matches!(
            KeyValueHolderCrypt::from_value(&[0; 28]),
            KeyValueHolderCrypt::Memory(_)
        ));
        assert!(!KeyValueHolderCrypt::is_value_stored_as_offset(256));
        assert!(KeyValueHolderCrypt::is_value_stored_as_offset(257));
    }

    #[test]
    fn offset_holder_layout() {
        let holder = KeyValueHolderCrypt::offset(
            3,
            500,
            4,
            AesStatus {
                num: 4,
                ivec: [1; 16],
            },
        )
        .unwrap();
        match holder {
            KeyValueHolderCrypt::Offset {
                pb_key_value_size,
                key_size,
                value_size,
                offset,
                ..
            } => {
                assert_eq!(pb_key_value_size, 3); // varint(3)=1 + varint(500)=2
                assert_eq!(key_size, 3);
                assert_eq!(value_size, 500);
                assert_eq!(offset, 4);
            }
            _ => panic!("expected offset holder"),
        }
    }
}
