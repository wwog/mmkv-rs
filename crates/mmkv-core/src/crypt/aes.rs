//! AES-CFB128 encryption compatible with `Core/aes/AESCrypt.cpp`.
//!
//! MMKV uses one continuous CFB-128 stream over the whole payload. The cipher
//! state is `(ivec[16], num)`; `num` is always `stream_offset % 16`. Both
//! encryption and decryption feed back the ciphertext byte.

use aes::cipher::{BlockEncrypt, KeyInit, generic_array::GenericArray};
use aes::{Aes128, Aes256};

pub const AES_KEY_LEN: usize = 16;
pub const AES_IV_LEN: usize = 16;
pub const AES256_KEY_LEN: usize = 32;

/// `AESCryptStatus` from `AESCrypt.h`: 17 bytes, memory-only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AesStatus {
    pub num: u8,
    pub ivec: [u8; AES_IV_LEN],
}

#[allow(clippy::large_enum_variant)]
enum Cipher {
    Aes128(Aes128),
    Aes256(Aes256),
}

impl Cipher {
    fn encrypt_block(&self, block: &mut [u8; AES_IV_LEN]) {
        let mut generic = GenericArray::clone_from_slice(block);
        match self {
            Cipher::Aes128(cipher) => cipher.encrypt_block(&mut generic),
            Cipher::Aes256(cipher) => cipher.encrypt_block(&mut generic),
        }
        block.copy_from_slice(&generic);
    }
}

/// A CFB-128 stream with an AES-128/256 key schedule.
pub struct AesCrypt {
    cipher: Cipher,
    key: [u8; AES256_KEY_LEN],
    key_len: usize,
    aes256: bool,
    num: u8,
    ivec: [u8; AES_IV_LEN],
    initial_iv: [u8; AES_IV_LEN],
}

impl std::fmt::Debug for AesCrypt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AesCrypt")
            .field("aes256", &self.aes256)
            .field("num", &self.num)
            .finish_non_exhaustive()
    }
}

impl Drop for AesCrypt {
    fn drop(&mut self) {
        self.key.fill(0);
        self.ivec.fill(0);
        self.initial_iv.fill(0);
    }
}

impl AesCrypt {
    /// MMKV uses the raw key bytes, truncated or zero-padded to 16/32 bytes.
    pub fn new(key: &[u8], aes256: bool) -> Self {
        let max_len = if aes256 { AES256_KEY_LEN } else { AES_KEY_LEN };
        let mut material = [0u8; AES256_KEY_LEN];
        let key_len = key.len().min(max_len);
        material[..key_len].copy_from_slice(&key[..key_len]);

        let cipher = if aes256 {
            Cipher::Aes256(Aes256::new(GenericArray::from_slice(&material)))
        } else {
            Cipher::Aes128(Aes128::new(GenericArray::from_slice(
                &material[..AES_KEY_LEN],
            )))
        };

        let mut crypt = Self {
            cipher,
            key: material,
            key_len,
            aes256,
            num: 0,
            ivec: [0; AES_IV_LEN],
            initial_iv: [0; AES_IV_LEN],
        };
        crypt.reset_iv(None);
        crypt
    }

    pub fn is_aes256(&self) -> bool {
        self.aes256
    }

    pub fn max_key_length(&self) -> usize {
        if self.aes256 {
            AES256_KEY_LEN
        } else {
            AES_KEY_LEN
        }
    }

    /// The configured key bytes (without zero padding).
    pub fn key(&self) -> &[u8] {
        &self.key[..self.key_len]
    }

    pub fn matches_key(&self, key: &[u8], aes256: bool) -> bool {
        self.aes256 == aes256 && self.key() == key
    }

    /// Reset the stream. `None` reproduces MMKV's legacy behaviour: the IV is
    /// the first 16 bytes of the key material (used by files with
    /// `m_version < MMKVVersionRandomIV`).
    pub fn reset_iv(&mut self, iv: Option<&[u8]>) {
        match iv {
            Some(iv) => {
                let length = iv.len().min(AES_IV_LEN);
                self.ivec = [0; AES_IV_LEN];
                self.ivec[..length].copy_from_slice(&iv[..length]);
            }
            None => {
                self.ivec.copy_from_slice(&self.key[..AES_IV_LEN]);
            }
        }
        self.initial_iv = self.ivec;
        self.num = 0;
    }

    pub fn status(&self) -> AesStatus {
        AesStatus {
            num: self.num,
            ivec: self.ivec,
        }
    }

    pub fn set_status(&mut self, status: &AesStatus) {
        self.num = status.num & 0x0f;
        self.ivec = status.ivec;
    }

    pub fn clone_with_status(&self, status: &AesStatus) -> Self {
        let mut clone = Self {
            cipher: match &self.cipher {
                Cipher::Aes128(cipher) => Cipher::Aes128(cipher.clone()),
                Cipher::Aes256(cipher) => Cipher::Aes256(cipher.clone()),
            },
            key: self.key,
            key_len: self.key_len,
            aes256: self.aes256,
            num: 0,
            ivec: self.ivec,
            initial_iv: self.initial_iv,
        };
        clone.set_status(status);
        clone
    }

    /// Encrypt in place (plaintext in, ciphertext out).
    pub fn encrypt(&mut self, data: &mut [u8]) {
        let mut num = self.num as usize;
        for byte in data.iter_mut() {
            if num == 0 {
                self.cipher.encrypt_block(&mut self.ivec);
            }
            let cipher = *byte ^ self.ivec[num];
            self.ivec[num] = cipher;
            *byte = cipher;
            num = (num + 1) & 0x0f;
        }
        self.num = num as u8;
    }

    /// Decrypt in place (ciphertext in, plaintext out).
    pub fn decrypt(&mut self, data: &mut [u8]) {
        let mut num = self.num as usize;
        for byte in data.iter_mut() {
            if num == 0 {
                self.cipher.encrypt_block(&mut self.ivec);
            }
            let cipher = *byte;
            *byte = cipher ^ self.ivec[num];
            self.ivec[num] = cipher;
            num = (num + 1) & 0x0f;
        }
        self.num = num as u8;
    }

    /// Decrypt `input` into `output` without aliasing.
    pub fn decrypt_to(&mut self, input: &[u8], output: &mut [u8]) {
        debug_assert_eq!(input.len(), output.len());
        let mut num = self.num as usize;
        for (index, &cipher) in input.iter().enumerate() {
            if num == 0 {
                self.cipher.encrypt_block(&mut self.ivec);
            }
            output[index] = cipher ^ self.ivec[num];
            self.ivec[num] = cipher;
            num = (num + 1) & 0x0f;
        }
        self.num = num as u8;
    }

    /// Reconstruct the CFB state at `offset` from the ciphertext stream.
    ///
    /// This is O(1) block encryptions and removes the need for MMKV's
    /// `statusBeforeDecrypt` rollback (we never decrypt ahead).
    pub fn status_at(&self, ciphertext: &[u8], offset: usize) -> AesStatus {
        if offset == 0 {
            return AesStatus {
                num: 0,
                ivec: self.initial_iv,
            };
        }
        let num = offset & 0x0f;
        let mut ivec = [0u8; AES_IV_LEN];
        if offset < AES_IV_LEN {
            ivec = self.initial_iv;
            self.cipher.encrypt_block(&mut ivec);
            ivec[..num].copy_from_slice(&ciphertext[..num]);
        } else {
            let block_start = offset - num - AES_IV_LEN;
            ivec.copy_from_slice(&ciphertext[block_start..block_start + AES_IV_LEN]);
            // At a block boundary `ivec` is the previous ciphertext block; the
            // cipher only advances when the next byte is processed. Mid-block
            // it holds the keystream block with the ciphertext prefix.
            if num > 0 {
                self.cipher.encrypt_block(&mut ivec);
                ivec[..num].copy_from_slice(&ciphertext[offset - num..offset]);
            }
        }
        AesStatus {
            num: num as u8,
            ivec,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn nist_aes128_cfb128_vector() {
        // NIST SP 800-38A, F.3.13 (first block).
        let key = hex_to_bytes("2b7e151628aed2a6abf7158809cf4f3c");
        let iv = hex_to_bytes("000102030405060708090a0b0c0d0e0f");
        let plaintext = hex_to_bytes("6bc1bee22e409f96e93d7e117393172a");
        let expected = "3b3fd92eb72dad20333449f8e83cfb4a";

        let mut crypt = AesCrypt::new(&key, false);
        crypt.reset_iv(Some(&iv));
        let mut data = plaintext.clone();
        crypt.encrypt(&mut data);
        assert_eq!(hex(&data), expected);

        let mut decrypt = AesCrypt::new(&key, false);
        decrypt.reset_iv(Some(&iv));
        decrypt.decrypt(&mut data);
        assert_eq!(data, plaintext);
    }

    #[test]
    fn status_at_matches_sequential_state() {
        let key = b"0123456789abcdef";
        let iv = [7u8; 16];
        let plaintext: Vec<u8> = (0..200).map(|index| index as u8).collect();

        let mut sequential = AesCrypt::new(key, false);
        sequential.reset_iv(Some(&iv));
        let mut ciphertext = plaintext.clone();
        sequential.encrypt(&mut ciphertext);

        for offset in [0usize, 1, 5, 15, 16, 17, 31, 32, 100, 200] {
            let status = sequential.status_at(&ciphertext, offset);
            let mut decrypt = sequential.clone_with_status(&status);
            let mut output = ciphertext[offset..].to_vec();
            decrypt.decrypt(&mut output);
            assert_eq!(&output[..], &plaintext[offset..], "offset {offset}");
        }
    }

    #[test]
    fn key_padding_matches_mmkv() {
        let crypt = AesCrypt::new(b"short", false);
        assert_eq!(crypt.key(), b"short");
        assert_eq!(crypt.max_key_length(), 16);
        let crypt = AesCrypt::new(b"0123456789abcdefEXTRA", true);
        assert_eq!(crypt.key(), b"0123456789abcdefEXTRA");
        assert_eq!(crypt.max_key_length(), 32);
    }

    fn hex_to_bytes(value: &str) -> Vec<u8> {
        (0..value.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&value[index..index + 2], 16).unwrap())
            .collect()
    }
}
