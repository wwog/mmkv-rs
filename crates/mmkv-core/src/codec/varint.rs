//! Protobuf varint sizing and encoding helpers (`PBUtility`).

pub const MAX_VARINT32_SIZE: usize = 5;
pub const MAX_VARINT64_SIZE: usize = 10;

pub const fn varint_size_u32(mut value: u32) -> usize {
    let mut size = 1;
    while value >= 0x80 {
        value >>= 7;
        size += 1;
    }
    size
}

pub const fn varint_size_u64(mut value: u64) -> usize {
    let mut size = 1;
    while value >= 0x80 {
        value >>= 7;
        size += 1;
    }
    size
}

/// `pbInt32Size`: negative values are sign-extended to ten bytes.
pub const fn varint_size_i32(value: i32) -> usize {
    if value >= 0 {
        varint_size_u32(value as u32)
    } else {
        MAX_VARINT64_SIZE
    }
}

pub const fn varint_size_i64(value: i64) -> usize {
    varint_size_u64(value as u64)
}

pub fn append_varint32(out: &mut Vec<u8>, mut value: u32) {
    while value >= 0x80 {
        out.push(((value & 0x7f) | 0x80) as u8);
        value >>= 7;
    }
    out.push(value as u8);
}

pub fn append_varint64(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(((value & 0x7f) | 0x80) as u8);
        value >>= 7;
    }
    out.push(value as u8);
}

pub fn append_int32(out: &mut Vec<u8>, value: i32) {
    if value >= 0 {
        append_varint32(out, value as u32);
    } else {
        append_varint64(out, value as i64 as u64);
    }
}

pub fn append_int64(out: &mut Vec<u8>, value: i64) {
    append_varint64(out, value as u64);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_sizes_match_protobuf() {
        assert_eq!(varint_size_u32(0), 1);
        assert_eq!(varint_size_u32(127), 1);
        assert_eq!(varint_size_u32(128), 2);
        assert_eq!(varint_size_u32(16_383), 2);
        assert_eq!(varint_size_u32(16_384), 3);
        assert_eq!(varint_size_u32(u32::MAX), 5);
        assert_eq!(varint_size_u64(u64::MAX), 10);
        assert_eq!(varint_size_i32(-1), 10);
        assert_eq!(varint_size_i32(i32::MIN), 10);
        assert_eq!(varint_size_u32(0x20_0000), 4);
    }

    #[test]
    fn encode_negative_i32_is_ten_bytes() {
        let mut out = Vec::new();
        append_int32(&mut out, -1);
        assert_eq!(out.len(), 10);
        assert_eq!(
            out,
            [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]
        );
    }
}
