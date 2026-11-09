//! In-memory key/value holders (`KeyValueHolder`).

use crate::codec::varint::varint_size_u32;

/// `KeyValueHolder.h`: `keySize + varint(keySize) + varint(valueSize)` must fit
/// in a `u16`, hence the limit.
pub const KEY_SIZE_LIMIT: u32 = u16::MAX as u32 - 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyValueHolder {
    /// Bytes from the start of the record to the value: key-size varint +
    /// key + value-size varint.
    pub computed_kv_size: u16,
    pub key_size: u16,
    pub value_size: u32,
    /// Offset of the record start, relative to the payload (file offset 4).
    pub offset: u32,
}

impl KeyValueHolder {
    pub fn new(key_size: u32, value_size: u32, offset: u32) -> Option<Self> {
        if key_size > KEY_SIZE_LIMIT {
            return None;
        }
        let computed = key_size as usize + varint_size_u32(key_size) + varint_size_u32(value_size);
        if computed > u16::MAX as usize {
            return None;
        }
        Some(Self {
            computed_kv_size: computed as u16,
            key_size: key_size as u16,
            value_size,
            offset,
        })
    }

    /// Slice the value out of the payload. `payload` starts at file offset 4.
    pub fn value<'a>(&self, payload: &'a [u8]) -> Option<&'a [u8]> {
        let start = (self.offset as usize).checked_add(self.computed_kv_size as usize)?;
        let end = start.checked_add(self.value_size as usize)?;
        payload.get(start..end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computed_size_matches_layout() {
        let holder = KeyValueHolder::new(3, 5, 0).unwrap();
        // 1 (key len) + 3 (key) + 1 (value len) = 5
        assert_eq!(holder.computed_kv_size, 5);
        let payload = b"\x03abc\x05value";
        assert_eq!(holder.value(payload), Some(b"value".as_slice()));
    }
}
