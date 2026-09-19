//! Message framing: one newline-delimited JSON value per message, bounded.
//!
//! Framing is separated from both the protocol and the transport so the size
//! cap and the truncation rules are enforced in exactly one place, whatever
//! carries the bytes.

use std::io;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use crate::protocol::MAX_REQUEST_BYTES;

/// What came off the wire.
#[derive(Debug, PartialEq, Eq)]
pub enum Frame {
    /// A complete newline-terminated line.
    Line(String),
    /// The peer closed without sending anything.
    Empty,
    /// The peer exceeded [`MAX_REQUEST_BYTES`] before sending a newline.
    ///
    /// Distinguished from `Line` so the caller answers "too large" rather than
    /// trying to parse a truncated prefix as JSON, which would produce a
    /// misleading syntax error.
    TooLarge,
}

/// Read one frame, refusing to buffer more than `limit` bytes.
///
/// A client that connects and never sends a newline would otherwise make the
/// server buffer without bound.
pub async fn read_frame<R>(reader: R, limit: u64) -> io::Result<Frame>
where
    R: AsyncRead + Unpin,
{
    // `take` bounds the read itself; hitting the bound exactly is how an
    // over-limit request is detected, because a compliant request is strictly
    // shorter than the cap once its newline is counted.
    let mut buf_reader = BufReader::new(reader.take(limit));
    let mut line = String::new();
    let read = buf_reader.read_line(&mut line).await?;

    if read == 0 {
        return Ok(Frame::Empty);
    }
    if read as u64 >= limit {
        return Ok(Frame::TooLarge);
    }
    Ok(Frame::Line(line))
}

/// Read one frame at the protocol's default cap.
pub async fn read_request_frame<R>(reader: R) -> io::Result<Frame>
where
    R: AsyncRead + Unpin,
{
    read_frame(reader, MAX_REQUEST_BYTES).await
}

/// Write one frame, appending the delimiter.
pub async fn write_frame<W>(writer: &mut W, payload: &str) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(payload.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
}

/// Serialise and write one JSON frame.
pub async fn write_json_frame<W, T>(writer: &mut W, value: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: serde::Serialize,
{
    let json =
        serde_json::to_string(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    write_frame(writer, &json).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_a_complete_line() {
        let input = b"{\"a\":1}\n".to_vec();
        let frame = read_frame(&input[..], 1024).await.unwrap();
        assert_eq!(frame, Frame::Line("{\"a\":1}\n".to_string()));
    }

    #[tokio::test]
    async fn an_immediate_close_is_empty_not_an_error() {
        let input: Vec<u8> = Vec::new();
        assert_eq!(read_frame(&input[..], 1024).await.unwrap(), Frame::Empty);
    }

    #[tokio::test]
    async fn an_oversized_request_is_reported_as_too_large() {
        // 64 bytes with no newline against a 32-byte cap.
        let input = [b'x'; 64];
        assert_eq!(read_frame(&input[..], 32).await.unwrap(), Frame::TooLarge);
    }

    #[tokio::test]
    async fn a_request_exactly_at_the_cap_is_too_large_rather_than_truncated() {
        // Exactly `limit` bytes: the newline, if any, is beyond the cap, so
        // the prefix must not be handed on as if it were complete.
        let input = [b'x'; 32];
        assert_eq!(read_frame(&input[..], 32).await.unwrap(), Frame::TooLarge);
    }

    #[tokio::test]
    async fn a_request_just_under_the_cap_still_reads() {
        let mut input = vec![b'x'; 30];
        input.push(b'\n');
        match read_frame(&input[..], 32).await.unwrap() {
            Frame::Line(l) => assert_eq!(l.len(), 31),
            other => panic!("expected a line, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn write_appends_exactly_one_delimiter() {
        let mut out: Vec<u8> = Vec::new();
        write_frame(&mut out, "{\"ok\":true}").await.unwrap();
        assert_eq!(out, b"{\"ok\":true}\n");
    }

    #[tokio::test]
    async fn a_frame_survives_a_roundtrip() {
        let mut out: Vec<u8> = Vec::new();
        write_json_frame(&mut out, &serde_json::json!({"cmd": "status"}))
            .await
            .unwrap();
        match read_frame(&out[..], 1024).await.unwrap() {
            Frame::Line(l) => {
                let v: serde_json::Value = serde_json::from_str(&l).unwrap();
                assert_eq!(v["cmd"], "status");
            }
            other => panic!("expected a line, got {other:?}"),
        }
    }
}
