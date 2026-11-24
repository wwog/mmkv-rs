//! Encrypted value holders (`KeyValueHolderCrypt`).
//!
//! * `Direct`: plaintext cached inline, up to 27 bytes.
//! * `Memory`: plaintext cached on the heap, 28..=256 bytes.
//! * `Offset`: value stays encrypted in the mapped file; decrypted on demand
//!   using the CFB status captured at the record start.

use std::borrow::Cow;

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

    /// Same tiers as `from_value`, but takes ownership of the buffer.
    pub fn from_owned(value: Vec<u8>) -> Self {
        if value.len() <= SMALL_BUFFER_SIZE {
            Self::Direct(value)
        } else {
            Self::Memory(value)
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

    /// Expiration trailer (`u32 LE`) of the value.
    ///
    /// Cached values are read in place. Offset-held ciphertext only needs the
    /// four trailer bytes decrypted, using the O(1) CFB status reconstruction,
    /// instead of decrypting the whole value to inspect its tail.
    ///
    /// `None` when the value is too short to carry a trailer.
    pub fn expiry(&self, payload: &[u8], crypter: &AesCrypt) -> Option<u32> {
        match self {
            Self::Direct(value) | Self::Memory(value) => {
                let start = value.len().checked_sub(4)?;
                Some(u32::from_le_bytes(value[start..].try_into().ok()?))
            }
            Self::Offset {
                pb_key_value_size,
                key_size,
                value_size,
                offset,
                ..
            } => {
                let start = (*offset as usize)
                    .checked_add(*pb_key_value_size as usize)?
                    .checked_add(*key_size as usize)?;
                let end = start.checked_add(*value_size as usize)?;
                let trailer = end.checked_sub(4)?;
                if end > payload.len() {
                    return None;
                }
                let mut output = [0u8; 4];
                crypter.decrypt_at(payload, trailer, &mut output);
                Some(u32::from_le_bytes(output))
            }
        }
    }

    /// Decrypt (or return the cached) value blob. The returned bytes are the
    /// value as stored on disk, i.e. what MMKV's getters expect.
    ///
    /// Offset holders decrypt straight from the mapping at the value start, so
    /// no cipher clone and no prefix decryption are needed.
    pub fn value<'a>(&'a self, payload: &'a [u8], crypter: &AesCrypt) -> Option<Cow<'a, [u8]>> {
        match self {
            Self::Direct(value) | Self::Memory(value) => Some(Cow::Borrowed(value)),
            Self::Offset {
                pb_key_value_size,
                key_size,
                value_size,
                offset,
                ..
            } => {
                let start = (*offset as usize)
                    .checked_add(*pb_key_value_size as usize)?
                    .checked_add(*key_size as usize)?;
                let end = start.checked_add(*value_size as usize)?;
                let ciphertext = payload.get(start..end)?;
                let mut plaintext = vec![0u8; ciphertext.len()];
                crypter.decrypt_at(payload, start, &mut plaintext);
                Some(Cow::Owned(plaintext))
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
    fn offset_expiry_decrypts_only_the_trailer() {
        use crate::codec::varint::append_varint32;

        let key = b"0123456789abcdef";
        let iv = [3u8; 16];
        let mut crypt = AesCrypt::new(key, false);
        crypt.reset_iv(Some(&iv));

        let record_key = b"expiring";
        let timestamp = 1_700_000_000u32;
        let mut value = vec![7u8; 300];
        value.extend_from_slice(&timestamp.to_le_bytes());
        let value_size = value.len() as u32;

        let mut payload = Vec::new();
        append_varint32(&mut payload, 0x20_0000);
        // The placeholder is part of the CFB stream, as in a real file.
        crypt.encrypt(&mut payload[0..4]);
        let status = crypt.status();
        append_varint32(&mut payload, record_key.len() as u32);
        payload.extend_from_slice(record_key);
        append_varint32(&mut payload, value_size);
        payload.extend_from_slice(&value);
        crypt.encrypt(&mut payload[4..]);

        let holder =
            KeyValueHolderCrypt::offset(record_key.len() as u32, value_size, 4, status).unwrap();
        assert_eq!(holder.expiry(&payload, &crypt), Some(timestamp));
        // The value is still readable through the normal path.
        assert_eq!(
            holder.value(&payload, &crypt).as_deref(),
            Some(value.as_slice())
        );

        let short = KeyValueHolderCrypt::from_value(&[1, 2, 3]);
        assert_eq!(short.expiry(&payload, &crypt), None);
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
