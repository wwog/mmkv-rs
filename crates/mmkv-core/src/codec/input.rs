//! Bounds-checked reader for MMKV's protobuf-style payload (`CodedInputData`).

use super::CodecError;
use super::CodecResult;

#[derive(Clone, Copy, Debug)]
pub struct CodedInput<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> CodedInput<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    pub fn position(&self) -> usize {
        self.position
    }

    pub fn is_at_end(&self) -> bool {
        self.position == self.data.len()
    }

    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.position)
    }

    pub fn seek(&mut self, position: usize) -> CodecResult<()> {
        if position > self.data.len() {
            return Err(CodecError::OutOfBounds);
        }
        self.position = position;
        Ok(())
    }

    pub fn skip(&mut self, count: usize) -> CodecResult<()> {
        let next = self
            .position
            .checked_add(count)
            .ok_or(CodecError::OutOfBounds)?;
        if next > self.data.len() {
            return Err(CodecError::OutOfBounds);
        }
        self.position = next;
        Ok(())
    }

    pub fn read_raw_byte(&mut self) -> CodecResult<u8> {
        let byte = *self
            .data
            .get(self.position)
            .ok_or(CodecError::OutOfBounds)?;
        self.position += 1;
        Ok(byte)
    }

    pub fn read_bytes(&mut self, count: usize) -> CodecResult<&'a [u8]> {
        let next = self
            .position
            .checked_add(count)
            .ok_or(CodecError::OutOfBounds)?;
        if next > self.data.len() {
            return Err(CodecError::OutOfBounds);
        }
        let slice = &self.data[self.position..next];
        self.position = next;
        Ok(slice)
    }

    /// Read a varint32. Mirrors `CodedInputData::readRawVarint32`, including
    /// the ten-byte encoding used for negative `int32` values: only the low
    /// 32 bits are retained.
    pub fn read_varint32(&mut self) -> CodecResult<u32> {
        let mut result = 0u32;
        for index in 0..10 {
            let byte = self.read_raw_byte()?;
            if index < 5 {
                result |= ((byte & 0x7f) as u32) << (7 * index);
            }
            if byte & 0x80 == 0 {
                return Ok(result);
            }
        }
        Err(CodecError::Malformed)
    }

    pub fn read_varint64(&mut self) -> CodecResult<u64> {
        let mut result = 0u64;
        for index in 0..10 {
            let byte = self.read_raw_byte()?;
            result |= ((byte & 0x7f) as u64) << (7 * index);
            if byte & 0x80 == 0 {
                return Ok(result);
            }
        }
        Err(CodecError::Malformed)
    }

    pub fn read_bool(&mut self) -> CodecResult<bool> {
        Ok(self.read_varint32()? != 0)
    }

    pub fn read_i32(&mut self) -> CodecResult<i32> {
        Ok(self.read_varint32()? as i32)
    }

    pub fn read_u32(&mut self) -> CodecResult<u32> {
        self.read_varint32()
    }

    pub fn read_i64(&mut self) -> CodecResult<i64> {
        Ok(self.read_varint64()? as i64)
    }

    pub fn read_u64(&mut self) -> CodecResult<u64> {
        self.read_varint64()
    }

    pub fn read_f32(&mut self) -> CodecResult<f32> {
        let bytes = self.read_bytes(4)?;
        Ok(f32::from_bits(u32::from_le_bytes(
            bytes.try_into().expect("length checked"),
        )))
    }

    pub fn read_f64(&mut self) -> CodecResult<f64> {
        let bytes = self.read_bytes(8)?;
        Ok(f64::from_bits(u64::from_le_bytes(
            bytes.try_into().expect("length checked"),
        )))
    }

    /// Read a length-delimited byte string (`CodedInputData::readData`).
    pub fn read_len_prefixed(&mut self) -> CodecResult<&'a [u8]> {
        let length = self.read_varint32()? as usize;
        self.read_bytes(length)
    }

    /// Read a length-delimited UTF-8 string (`CodedInputData::readString`).
    pub fn read_str(&mut self) -> CodecResult<&'a str> {
        let bytes = self.read_len_prefixed()?;
        std::str::from_utf8(bytes).map_err(|_| CodecError::Malformed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_varints() {
        for value in [0u32, 1, 127, 128, 16_383, 16_384, u32::MAX] {
            let mut buffer = Vec::new();
            super::super::varint::append_varint32(&mut buffer, value);
            let mut input = CodedInput::new(&buffer);
            assert_eq!(input.read_varint32().unwrap(), value);
            assert!(input.is_at_end());
        }
    }

    #[test]
    fn negative_int32_roundtrip() {
        for value in [-1i32, i32::MIN, -12345] {
            let mut buffer = Vec::new();
            super::super::varint::append_int32(&mut buffer, value);
            let mut input = CodedInput::new(&buffer);
            assert_eq!(input.read_i32().unwrap(), value);
        }
    }

    #[test]
    fn truncated_input_is_rejected() {
        let mut input = CodedInput::new(&[0x80]);
        assert_eq!(input.read_varint32(), Err(CodecError::OutOfBounds));
    }
}
