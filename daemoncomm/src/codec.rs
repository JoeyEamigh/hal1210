#![allow(dead_code)]
// The protocol consists of:
// - 7-byte magic number: b"HAL1210"
// - 3-byte big-endian length of the following messagepacked data
// - Messagepacked data

pub const MAGIC: &[u8; 7] = b"HAL1210";
pub const HEADER_LEN: usize = 7 + 3;
