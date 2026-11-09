//! Wire-format codec: protobuf-style varints and length-delimited values.
//!
//! This is a direct port of `CodedInputData`, `CodedOutputData`, `PBUtility`
//! and `MiniPBCoder`. The encoding is not self-describing: the caller (store)
//! knows the type it stored.

pub mod input;
pub mod mini_pb;
pub mod output;
pub mod varint;

pub use input::CodedInput;
pub use output::CodedOutput;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecError {
    /// Not enough bytes left in the buffer.
    OutOfBounds,
    /// Structurally invalid input (bad UTF-8, malformed varint, ...).
    Malformed,
    /// A computed length does not fit MMKV's on-disk representation.
    LengthOverflow,
}

pub type CodecResult<T> = std::result::Result<T, CodecError>;
