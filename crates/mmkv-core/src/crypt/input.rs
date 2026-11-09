//! Bounded-memory reader for an encrypted MMKV payload.
//!
//! Decrypts one CFB block at a time into a 16-byte window, so loading a large
//! store does not allocate a plaintext copy of the whole file. Mirrors the
//! streaming `CodedInputDataCrypt` in C++ without its read-ahead/rollback
//! complexity.

use crate::crypt::aes::AesCrypt;

pub struct CryptReader<'a> {
    ciphertext: &'a [u8],
    /// Bytes consumed by the parser.
    position: usize,
    /// Bytes decrypted into `window`.
    decrypt_position: usize,
    crypter: AesCrypt,
    window: [u8; 16],
    window_len: usize,
    window_pos: usize,
}

impl<'a> CryptReader<'a> {
    pub fn new(ciphertext: &'a [u8], start: usize, crypter: &AesCrypt) -> Option<Self> {
        if start > ciphertext.len() {
            return None;
        }
        let status = crypter.status_at(ciphertext, start);
        Some(Self {
            ciphertext,
            position: start,
            decrypt_position: start,
            crypter: crypter.clone_with_status(&status),
            window: [0u8; 16],
            window_len: 0,
            window_pos: 0,
        })
    }

    pub fn position(&self) -> usize {
        self.position
    }

    pub fn is_at_end(&self) -> bool {
        self.position >= self.ciphertext.len()
    }

    fn fill(&mut self) -> bool {
        if self.window_pos < self.window_len {
            return true;
        }
        if self.decrypt_position >= self.ciphertext.len() {
            return false;
        }
        let count = (self.ciphertext.len() - self.decrypt_position).min(16);
        let mut block = [0u8; 16];
        self.crypter.decrypt_to(
            &self.ciphertext[self.decrypt_position..self.decrypt_position + count],
            &mut block[..count],
        );
        self.window[..count].copy_from_slice(&block[..count]);
        self.decrypt_position += count;
        self.window_len = count;
        self.window_pos = 0;
        true
    }

    pub fn read_byte(&mut self) -> Option<u8> {
        if !self.fill() {
            return None;
        }
        let value = self.window[self.window_pos];
        self.window_pos += 1;
        self.position += 1;
        Some(value)
    }

    pub fn read_bytes(&mut self, count: usize) -> Option<Vec<u8>> {
        let mut output = Vec::with_capacity(count);
        while output.len() < count {
            if !self.fill() {
                return None;
            }
            let take = (count - output.len()).min(self.window_len - self.window_pos);
            output.extend_from_slice(&self.window[self.window_pos..self.window_pos + take]);
            self.window_pos += take;
            self.position += take;
        }
        Some(output)
    }

    pub fn skip(&mut self, count: usize) -> bool {
        let mut remaining = count;
        while remaining > 0 {
            if !self.fill() {
                return false;
            }
            let take = remaining.min(self.window_len - self.window_pos);
            self.window_pos += take;
            self.position += take;
            remaining -= take;
        }
        true
    }

    /// Protobuf varint32 with the same ten-byte negative-`int32` handling as
    /// `CodedInputData::readRawVarint32`.
    pub fn read_u32(&mut self) -> Option<u32> {
        let mut result = 0u32;
        for index in 0..10 {
            let byte = self.read_byte()?;
            if index < 5 {
                result |= ((byte & 0x7f) as u32) << (7 * index);
            }
            if byte & 0x80 == 0 {
                return Some(result);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_the_same_bytes_as_bulk_decrypt() {
        let key = b"0123456789abcdef";
        let iv = [3u8; 16];
        let plaintext: Vec<u8> = (0..500).map(|index| (index % 251) as u8).collect();

        let mut encrypter = AesCrypt::new(key, false);
        encrypter.reset_iv(Some(&iv));
        let mut ciphertext = plaintext.clone();
        encrypter.encrypt(&mut ciphertext);

        let mut reference = AesCrypt::new(key, false);
        reference.reset_iv(Some(&iv));
        let mut reader = CryptReader::new(&ciphertext, 0, &reference).unwrap();
        assert_eq!(reader.read_bytes(17).unwrap(), plaintext[..17]);
        assert!(reader.skip(100));
        assert_eq!(reader.read_bytes(23).unwrap(), plaintext[117..140]);
        assert_eq!(reader.position(), 140);
        assert_eq!(reader.read_bytes(360).unwrap(), plaintext[140..]);
        assert!(reader.is_at_end());
    }
}
