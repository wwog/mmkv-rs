//! `MiniPBCoder` equivalent: value payloads and simple containers.
//!
//! MMKV stores primitives as bare protobuf values, and containers/strings as
//! one length-delimited blob. Strings and byte arrays additionally go through
//! the "data holder" path in the store, which adds another length prefix on
//! disk; that is handled by `store::write`, not here.

use super::CodecResult;
use super::CodedInput;
use super::varint::{
    append_int32, append_varint32, append_varint64, varint_size_i32, varint_size_u32,
    varint_size_u64,
};

pub fn encode_bool(value: bool) -> Vec<u8> {
    vec![u8::from(value)]
}

pub fn encode_i32(value: i32) -> Vec<u8> {
    let mut out = Vec::with_capacity(varint_size_i32(value));
    append_int32(&mut out, value);
    out
}

pub fn encode_u32(value: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(varint_size_u32(value));
    append_varint32(&mut out, value);
    out
}

pub fn encode_i64(value: i64) -> Vec<u8> {
    let mut out = Vec::with_capacity(varint_size_u64(value as u64));
    append_varint64(&mut out, value as u64);
    out
}

pub fn encode_u64(value: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(varint_size_u64(value));
    append_varint64(&mut out, value);
    out
}

pub fn encode_f32(value: f32) -> Vec<u8> {
    value.to_bits().to_le_bytes().to_vec()
}

pub fn encode_f64(value: f64) -> Vec<u8> {
    value.to_bits().to_le_bytes().to_vec()
}

fn content_size_strings<S: AsRef<str>>(values: &[S]) -> usize {
    values
        .iter()
        .map(|value| {
            let len = value.as_ref().len();
            varint_size_u32(len as u32) + len
        })
        .sum()
}

/// `MiniPBCoder::encodeDataWithObject(vector<string>)`.
pub fn encode_strings<S: AsRef<str>>(values: &[S]) -> Vec<u8> {
    let content = content_size_strings(values);
    let mut out = Vec::with_capacity(varint_size_u32(content as u32) + content);
    append_varint32(&mut out, content as u32);
    for value in values {
        let bytes = value.as_ref().as_bytes();
        append_varint32(&mut out, bytes.len() as u32);
        out.extend_from_slice(bytes);
    }
    out
}

pub fn encode_bool_vec(values: &[bool]) -> Vec<u8> {
    let content = values.len();
    let mut out = Vec::with_capacity(varint_size_u32(content as u32) + content);
    append_varint32(&mut out, content as u32);
    out.extend(values.iter().map(|value| u8::from(*value)));
    out
}

pub fn encode_i32_vec(values: &[i32]) -> Vec<u8> {
    let content: usize = values.iter().map(|value| varint_size_i32(*value)).sum();
    let mut out = Vec::with_capacity(varint_size_u32(content as u32) + content);
    append_varint32(&mut out, content as u32);
    for value in values {
        append_int32(&mut out, *value);
    }
    out
}

pub fn encode_u32_vec(values: &[u32]) -> Vec<u8> {
    let content: usize = values.iter().map(|value| varint_size_u32(*value)).sum();
    let mut out = Vec::with_capacity(varint_size_u32(content as u32) + content);
    append_varint32(&mut out, content as u32);
    for value in values {
        append_varint32(&mut out, *value);
    }
    out
}

pub fn encode_i64_vec(values: &[i64]) -> Vec<u8> {
    let content: usize = values
        .iter()
        .map(|value| varint_size_u64(*value as u64))
        .sum();
    let mut out = Vec::with_capacity(varint_size_u32(content as u32) + content);
    append_varint32(&mut out, content as u32);
    for value in values {
        append_varint64(&mut out, *value as u64);
    }
    out
}

pub fn encode_u64_vec(values: &[u64]) -> Vec<u8> {
    let content: usize = values.iter().map(|value| varint_size_u64(*value)).sum();
    let mut out = Vec::with_capacity(varint_size_u32(content as u32) + content);
    append_varint32(&mut out, content as u32);
    for value in values {
        append_varint64(&mut out, *value);
    }
    out
}

pub fn encode_f32_vec(values: &[f32]) -> Vec<u8> {
    let content = values.len() * 4;
    let mut out = Vec::with_capacity(varint_size_u32(content as u32) + content);
    append_varint32(&mut out, content as u32);
    for value in values {
        out.extend_from_slice(&value.to_bits().to_le_bytes());
    }
    out
}

pub fn encode_f64_vec(values: &[f64]) -> Vec<u8> {
    let content = values.len() * 8;
    let mut out = Vec::with_capacity(varint_size_u32(content as u32) + content);
    append_varint32(&mut out, content as u32);
    for value in values {
        out.extend_from_slice(&value.to_bits().to_le_bytes());
    }
    out
}

pub fn decode_bool(blob: &[u8]) -> CodecResult<bool> {
    CodedInput::new(blob).read_bool()
}

pub fn decode_i32(blob: &[u8]) -> CodecResult<i32> {
    CodedInput::new(blob).read_i32()
}

pub fn decode_u32(blob: &[u8]) -> CodecResult<u32> {
    CodedInput::new(blob).read_u32()
}

pub fn decode_i64(blob: &[u8]) -> CodecResult<i64> {
    CodedInput::new(blob).read_i64()
}

pub fn decode_u64(blob: &[u8]) -> CodecResult<u64> {
    CodedInput::new(blob).read_u64()
}

pub fn decode_f32(blob: &[u8]) -> CodecResult<f32> {
    CodedInput::new(blob).read_f32()
}

pub fn decode_f64(blob: &[u8]) -> CodecResult<f64> {
    CodedInput::new(blob).read_f64()
}

pub fn decode_str(blob: &[u8]) -> CodecResult<&str> {
    CodedInput::new(blob).read_str()
}

pub fn decode_bytes(blob: &[u8]) -> CodecResult<&[u8]> {
    CodedInput::new(blob).read_len_prefixed()
}

pub fn decode_strings(blob: &[u8]) -> CodecResult<Vec<String>> {
    let mut input = CodedInput::new(blob);
    let _content = input.read_u32()?;
    let mut values = Vec::new();
    while !input.is_at_end() {
        values.push(input.read_str()?.to_owned());
    }
    Ok(values)
}

pub fn decode_bool_vec(blob: &[u8]) -> CodecResult<Vec<bool>> {
    let mut input = CodedInput::new(blob);
    let _content = input.read_u32()?;
    let mut values = Vec::new();
    while !input.is_at_end() {
        values.push(input.read_bool()?);
    }
    Ok(values)
}

pub fn decode_i32_vec(blob: &[u8]) -> CodecResult<Vec<i32>> {
    let mut input = CodedInput::new(blob);
    let _content = input.read_u32()?;
    let mut values = Vec::new();
    while !input.is_at_end() {
        values.push(input.read_i32()?);
    }
    Ok(values)
}

pub fn decode_u32_vec(blob: &[u8]) -> CodecResult<Vec<u32>> {
    let mut input = CodedInput::new(blob);
    let _content = input.read_u32()?;
    let mut values = Vec::new();
    while !input.is_at_end() {
        values.push(input.read_u32()?);
    }
    Ok(values)
}

pub fn decode_i64_vec(blob: &[u8]) -> CodecResult<Vec<i64>> {
    let mut input = CodedInput::new(blob);
    let _content = input.read_u32()?;
    let mut values = Vec::new();
    while !input.is_at_end() {
        values.push(input.read_i64()?);
    }
    Ok(values)
}

pub fn decode_u64_vec(blob: &[u8]) -> CodecResult<Vec<u64>> {
    let mut input = CodedInput::new(blob);
    let _content = input.read_u32()?;
    let mut values = Vec::new();
    while !input.is_at_end() {
        values.push(input.read_u64()?);
    }
    Ok(values)
}

pub fn decode_f32_vec(blob: &[u8]) -> CodecResult<Vec<f32>> {
    let mut input = CodedInput::new(blob);
    let _content = input.read_u32()?;
    let mut values = Vec::new();
    while !input.is_at_end() {
        values.push(input.read_f32()?);
    }
    Ok(values)
}

pub fn decode_f64_vec(blob: &[u8]) -> CodecResult<Vec<f64>> {
    let mut input = CodedInput::new(blob);
    let _content = input.read_u32()?;
    let mut values = Vec::new();
    while !input.is_at_end() {
        values.push(input.read_f64()?);
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_payload_is_length_delimited() {
        assert_eq!(encode_i32(0), [0]);
        assert_eq!(encode_i32(300), [0xac, 0x02]);
        assert_eq!(encode_f32(1.0), 1.0f32.to_le_bytes());
        assert_eq!(encode_f64(1.0), 1.0f64.to_le_bytes());
    }

    #[test]
    fn string_vectors_roundtrip() {
        let values = vec!["alpha".to_owned(), String::new(), "中文".to_owned()];
        let encoded = encode_strings(&values);
        assert_eq!(decode_strings(&encoded).unwrap(), values);
    }

    #[test]
    fn numeric_vectors_roundtrip() {
        let ints = vec![-1, 0, 1, i32::MIN];
        assert_eq!(decode_i32_vec(&encode_i32_vec(&ints)).unwrap(), ints);

        let floats = vec![1.5f64, -0.0, f64::INFINITY];
        assert_eq!(decode_f64_vec(&encode_f64_vec(&floats)).unwrap(), floats);
    }
}
