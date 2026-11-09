//! The 112-byte metadata structure stored at the start of `<store>.crc`.
//!
//! Layout must stay byte-compatible with `Core/MMKVMetaInfo.hpp`; the C++
//! struct is naturally aligned and all fields are little-endian on every
//! supported platform.

pub const META_SIZE: usize = 112;

pub const VERSION_DEFAULT: u32 = 0;
pub const VERSION_SEQUENCE: u32 = 1;
pub const VERSION_RANDOM_IV: u32 = 2;
pub const VERSION_ACTUAL_SIZE: u32 = 3;
pub const VERSION_FLAG: u32 = 4;
pub const VERSION_PRESERVED: u32 = 5;
pub const VERSION_NEXT: u32 = 6;
pub const VERSION_HOLDER: u32 = 7;

pub const FLAG_ENABLE_KEY_EXPIRE: u64 = 1 << 0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetaInfo {
    pub crc_digest: u32,
    pub version: u32,
    /// Full write-back counter.
    pub sequence: u32,
    /// AES IV used when `version >= VERSION_RANDOM_IV`.
    pub vector: [u8; 16],
    pub actual_size: u32,
    pub last_actual_size: u32,
    pub last_crc_digest: u32,
    pub reserved: [u32; 16],
    pub flags: u64,
}

impl Default for MetaInfo {
    fn default() -> Self {
        Self {
            crc_digest: 0,
            version: VERSION_DEFAULT,
            sequence: 0,
            vector: [0; 16],
            actual_size: 0,
            last_actual_size: 0,
            last_crc_digest: 0,
            reserved: [0; 16],
            flags: 0,
        }
    }
}

fn read_u32(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        buf[offset..offset + 4]
            .try_into()
            .expect("meta buffer is large enough"),
    )
}

fn write_u32(buf: &mut [u8], offset: usize, value: u32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

impl MetaInfo {
    pub fn read(buf: &[u8]) -> Option<Self> {
        if buf.len() < META_SIZE {
            return None;
        }
        let mut vector = [0u8; 16];
        vector.copy_from_slice(&buf[12..28]);
        let mut reserved = [0u32; 16];
        for (index, slot) in reserved.iter_mut().enumerate() {
            *slot = read_u32(buf, 40 + index * 4);
        }
        Some(Self {
            crc_digest: read_u32(buf, 0),
            version: read_u32(buf, 4),
            sequence: read_u32(buf, 8),
            vector,
            actual_size: read_u32(buf, 28),
            last_actual_size: read_u32(buf, 32),
            last_crc_digest: read_u32(buf, 36),
            reserved,
            flags: u64::from_le_bytes(
                buf[104..112]
                    .try_into()
                    .expect("meta buffer is large enough"),
            ),
        })
    }

    pub fn write(&self, buf: &mut [u8]) -> bool {
        if buf.len() < META_SIZE {
            return false;
        }
        write_u32(buf, 0, self.crc_digest);
        write_u32(buf, 4, self.version);
        write_u32(buf, 8, self.sequence);
        buf[12..28].copy_from_slice(&self.vector);
        write_u32(buf, 28, self.actual_size);
        write_u32(buf, 32, self.last_actual_size);
        write_u32(buf, 36, self.last_crc_digest);
        for (index, value) in self.reserved.iter().enumerate() {
            write_u32(buf, 40 + index * 4, *value);
        }
        buf[104..112].copy_from_slice(&self.flags.to_le_bytes());
        true
    }

    /// `MMKVMetaInfo::writeCRCAndActualSizeOnly`: appends only touch these two
    /// fields so the IV/version/sequence stay stable.
    pub fn write_crc_and_size(&self, buf: &mut [u8]) -> bool {
        if buf.len() < META_SIZE {
            return false;
        }
        write_u32(buf, 0, self.crc_digest);
        write_u32(buf, 28, self.actual_size);
        true
    }

    pub fn has_flag(&self, flag: u64) -> bool {
        self.flags & flag != 0
    }

    pub fn set_flag(&mut self, flag: u64) {
        self.flags |= flag;
    }

    pub fn unset_flag(&mut self, flag: u64) {
        self.flags &= !flag;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_and_offsets_match_cpp() {
        assert_eq!(META_SIZE, 112);
        let info = MetaInfo {
            crc_digest: 0x1122_3344,
            version: 4,
            sequence: 7,
            vector: [0xAA; 16],
            actual_size: 1234,
            last_actual_size: 1200,
            last_crc_digest: 0x5566_7788,
            flags: FLAG_ENABLE_KEY_EXPIRE,
            ..Default::default()
        };

        let mut buffer = [0u8; META_SIZE];
        assert!(info.write(&mut buffer));
        assert_eq!(&buffer[0..4], &0x1122_3344u32.to_le_bytes());
        assert_eq!(&buffer[4..8], &4u32.to_le_bytes());
        assert_eq!(&buffer[8..12], &7u32.to_le_bytes());
        assert_eq!(&buffer[12..28], &[0xAA; 16]);
        assert_eq!(&buffer[28..32], &1234u32.to_le_bytes());
        assert_eq!(&buffer[104..112], &1u64.to_le_bytes());
        assert_eq!(MetaInfo::read(&buffer).unwrap(), info);
    }
}
