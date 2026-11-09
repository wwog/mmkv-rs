//! zlib-compatible CRC-32, matching `Core/crc32/zlib/crc32.cpp`.
//!
//! Reflected polynomial `0xEDB88320`, init and final XOR `0xFFFFFFFF`.
//! MMKV chains calls by passing the previously returned digest as the new
//! seed, which is exactly what [`crc32`] supports.

const fn make_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut crc = index as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
}

static TABLE: [u32; 256] = make_table();

/// Continue a zlib CRC-32 computation. Pass `0` for the first chunk.
pub fn crc32(seed: u32, data: &[u8]) -> u32 {
    let mut crc = !seed;
    for &byte in data {
        crc = TABLE[((crc ^ byte as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vector() {
        assert_eq!(crc32(0, b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(0, b""), 0);
    }

    #[test]
    fn chaining_matches_single_shot() {
        let data = b"the quick brown fox jumps over the lazy dog";
        let split = 13;
        let chained = crc32(crc32(0, &data[..split]), &data[split..]);
        assert_eq!(chained, crc32(0, data));
    }
}
