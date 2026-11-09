//! Bounds-checked writer into a byte slice (`CodedOutputData`).

use super::CodecError;
use super::CodecResult;
use super::varint::{MAX_VARINT64_SIZE, varint_size_u32};

pub struct CodedOutput<'a> {
    buffer: &'a mut [u8],
    position: usize,
}

impl<'a> CodedOutput<'a> {
    pub fn new(buffer: &'a mut [u8]) -> Self {
        Self {
            buffer,
            position: 0,
        }
    }

    pub fn position(&self) -> usize {
        self.position
    }

    pub fn set_position(&mut self, position: usize) -> CodecResult<()> {
        if position > self.buffer.len() {
            return Err(CodecError::OutOfBounds);
        }
        self.position = position;
        Ok(())
    }

    pub fn seek(&mut self, added: usize) -> CodecResult<()> {
        self.require(added)?;
        self.position += added;
        Ok(())
    }

    pub fn space_left(&self) -> usize {
        self.buffer.len().saturating_sub(self.position)
    }

    pub fn cur_write_pointer(&mut self) -> &mut [u8] {
        &mut self.buffer[self.position..]
    }

    fn require(&self, length: usize) -> CodecResult<()> {
        if length > self.buffer.len().saturating_sub(self.position) {
            Err(CodecError::OutOfBounds)
        } else {
            Ok(())
        }
    }

    pub fn write_raw_byte(&mut self, value: u8) -> CodecResult<()> {
        self.require(1)?;
        self.buffer[self.position] = value;
        self.position += 1;
        Ok(())
    }

    pub fn write_raw(&mut self, data: &[u8]) -> CodecResult<()> {
        self.require(data.len())?;
        self.buffer[self.position..self.position + data.len()].copy_from_slice(data);
        self.position += data.len();
        Ok(())
    }

    pub fn write_varint32(&mut self, mut value: u32) -> CodecResult<()> {
        self.require(varint_size_u32(value).max(1))?;
        while value >= 0x80 {
            self.buffer[self.position] = ((value & 0x7f) | 0x80) as u8;
            self.position += 1;
            value >>= 7;
        }
        self.buffer[self.position] = value as u8;
        self.position += 1;
        Ok(())
    }

    pub fn write_varint64(&mut self, mut value: u64) -> CodecResult<()> {
        self.require(MAX_VARINT64_SIZE.min(1))?;
        while value >= 0x80 {
            self.require(1)?;
            self.buffer[self.position] = ((value & 0x7f) | 0x80) as u8;
            self.position += 1;
            value >>= 7;
        }
        self.require(1)?;
        self.buffer[self.position] = value as u8;
        self.position += 1;
        Ok(())
    }

    pub fn write_i32(&mut self, value: i32) -> CodecResult<()> {
        if value >= 0 {
            self.write_varint32(value as u32)
        } else {
            self.write_varint64(value as i64 as u64)
        }
    }

    pub fn write_u32(&mut self, value: u32) -> CodecResult<()> {
        self.write_varint32(value)
    }

    pub fn write_i64(&mut self, value: i64) -> CodecResult<()> {
        self.write_varint64(value as u64)
    }

    pub fn write_u64(&mut self, value: u64) -> CodecResult<()> {
        self.write_varint64(value)
    }

    pub fn write_bool(&mut self, value: bool) -> CodecResult<()> {
        self.write_raw_byte(if value { 1 } else { 0 })
    }

    pub fn write_f32(&mut self, value: f32) -> CodecResult<()> {
        self.write_raw(&value.to_bits().to_le_bytes())
    }

    pub fn write_f64(&mut self, value: f64) -> CodecResult<()> {
        self.write_raw(&value.to_bits().to_le_bytes())
    }

    pub fn write_len_prefixed(&mut self, data: &[u8]) -> CodecResult<()> {
        if data.len() > u32::MAX as usize {
            return Err(CodecError::LengthOverflow);
        }
        self.write_varint32(data.len() as u32)?;
        self.write_raw(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_negative_i32_as_ten_bytes() {
        let mut buffer = [0u8; 16];
        let mut output = CodedOutput::new(&mut buffer);
        output.write_i32(-1).unwrap();
        assert_eq!(output.position(), 10);
        assert_eq!(
            &buffer[..10],
            &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]
        );
    }

    #[test]
    fn out_of_bounds_is_rejected() {
        let mut buffer = [0u8; 2];
        let mut output = CodedOutput::new(&mut buffer);
        assert_eq!(output.write_raw(&[1, 2, 3]), Err(CodecError::OutOfBounds));
    }
}
