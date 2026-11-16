//! Versioned newline-delimited JSON protocol. stdout is protocol-only;
//! plugin diagnostics belong on stderr.
use super::api::{Aid, ApiResult, Notice, Request};
use serde::{Deserialize, Serialize};
use std::io::{self, BufRead, Write};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 1_048_576;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PluginFrame {
    Ready { protocol_version: u32, name: String },
    Activated,
    Request { id: u64, request: Request },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum KernelFrame {
    Welcome {
        protocol_version: u32,
        aid: Aid,
        generation: u64,
    },
    Active,
    Response {
        id: u64,
        result: ApiResult,
    },
    Notice {
        notice: Notice,
    },
    Shutdown {
        reason: String,
    },
}

/// Bound allocation before JSON parsing. Never uses an unbounded read_line.
pub fn read_frame<R: BufRead>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    loop {
        let input = reader.fill_buf()?;
        if input.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unterminated protocol frame",
                ))
            };
        }
        let newline = input.iter().position(|b| *b == b'\n');
        let take = newline.map_or(input.len(), |n| n + 1);
        if bytes.len().saturating_add(take) > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "protocol frame too large",
            ));
        }
        bytes.extend_from_slice(&input[..take]);
        reader.consume(take);
        if newline.is_some() {
            return Ok(Some(bytes));
        }
    }
}

pub fn write_frame<W: Write, T: Serialize>(writer: &mut W, frame: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(frame)?;
    if bytes.len().saturating_add(1) > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "protocol frame too large",
        ));
    }
    writer.write_all(&bytes)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    #[test]
    fn framing_rejects_oversize_and_partial_json() {
        assert!(read_frame(&mut Cursor::new(vec![b'x'; MAX_FRAME_BYTES + 1])).is_err());
        assert!(read_frame(&mut Cursor::new(b"{}".to_vec())).is_err());
        assert_eq!(
            read_frame(&mut Cursor::new(b"{}\n".to_vec())).unwrap(),
            Some(b"{}\n".to_vec())
        );
    }
}
