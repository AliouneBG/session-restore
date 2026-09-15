//! Length-prefixed message framing.
//!
//! Both hops of the IPC chain use the same framing so the relay is close to a memcpy:
//!
//! ```text
//! Extension --(native messaging: 4-byte native-endian length + UTF-8 JSON)--> Relay
//! Relay     --(named pipe: same framing)-->                                   Agent
//! ```
//!
//! Chrome's native messaging protocol specifies the length prefix in *native* byte
//! order. Every Windows target we support is little-endian, so this uses LE explicitly
//! rather than relying on the host's endianness being what we expect.

use std::io::{self, Read, Write};

/// Chrome refuses to send a message larger than this from an extension, so anything
/// bigger indicates a desynchronized stream rather than a legitimately large message.
pub const MAX_INBOUND_BYTES: u32 = 1024 * 1024;

/// Chrome accepts larger messages *to* an extension, but a restore payload that big
/// means something has gone wrong upstream. Bounded to keep a corrupt length prefix
/// from turning into a huge allocation.
pub const MAX_OUTBOUND_BYTES: u32 = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    /// The peer closed cleanly between messages. Expected, not an error condition:
    /// the browser exiting closes the relay's stdin, and MV3 service workers
    /// disconnect routinely (docs/05-ipc-protocol.md).
    #[error("stream closed")]
    Closed,

    #[error("frame of {len} bytes exceeds the {max} byte limit")]
    TooLarge { len: u32, max: u32 },

    #[error("frame is not valid UTF-8")]
    NotUtf8,
}

/// Reads one length-prefixed frame.
///
/// Returns `Closed` if the stream ends cleanly at a frame boundary. A stream that ends
/// *mid-frame* is a truncation and surfaces as an io error, because silently treating a
/// half-written message as a clean shutdown would hide real bugs.
pub fn read_frame<R: Read>(r: &mut R, max: u32) -> Result<String, FrameError> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Err(FrameError::Closed),
        Err(e) => return Err(FrameError::Io(e)),
    }

    let len = u32::from_le_bytes(len_buf);
    if len > max {
        return Err(FrameError::TooLarge { len, max });
    }
    // A zero-length frame is not meaningful JSON; treat it as a protocol desync.
    if len == 0 {
        return Err(FrameError::NotUtf8);
    }

    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|_| FrameError::NotUtf8)
}

/// Writes one length-prefixed frame and flushes.
///
/// Flushing matters: the relay is a pipe between two processes that are both waiting on
/// each other, so a buffered write that never leaves is an invisible deadlock.
pub fn write_frame<W: Write>(w: &mut W, payload: &str, max: u32) -> Result<(), FrameError> {
    let bytes = payload.as_bytes();
    let len: u32 = bytes
        .len()
        .try_into()
        .map_err(|_| FrameError::TooLarge { len: u32::MAX, max })?;
    if len > max {
        return Err(FrameError::TooLarge { len, max });
    }
    w.write_all(&len.to_le_bytes())?;
    w.write_all(bytes)?;
    w.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn roundtrip(s: &str) -> String {
        let mut buf = Vec::new();
        write_frame(&mut buf, s, MAX_OUTBOUND_BYTES).unwrap();
        read_frame(&mut Cursor::new(buf), MAX_INBOUND_BYTES).unwrap()
    }

    #[test]
    fn roundtrips_json() {
        let s = r#"{"v":1,"type":"hello","body":{}}"#;
        assert_eq!(roundtrip(s), s);
    }

    #[test]
    fn roundtrips_non_ascii() {
        // Window and page titles routinely contain non-ASCII; a byte-length vs
        // char-length mix-up here would corrupt every frame containing one.
        let s = r#"{"title":"Onimusha — 日本語 🎮"}"#;
        assert_eq!(roundtrip(s), s);
    }

    #[test]
    fn writes_little_endian_length_prefix() {
        let mut buf = Vec::new();
        write_frame(&mut buf, "ab", MAX_OUTBOUND_BYTES).unwrap();
        assert_eq!(&buf, &[2, 0, 0, 0, b'a', b'b']);
    }

    #[test]
    fn reads_consecutive_frames() {
        let mut buf = Vec::new();
        write_frame(&mut buf, "one", MAX_OUTBOUND_BYTES).unwrap();
        write_frame(&mut buf, "two", MAX_OUTBOUND_BYTES).unwrap();
        let mut c = Cursor::new(buf);
        assert_eq!(read_frame(&mut c, MAX_INBOUND_BYTES).unwrap(), "one");
        assert_eq!(read_frame(&mut c, MAX_INBOUND_BYTES).unwrap(), "two");
        assert!(matches!(
            read_frame(&mut c, MAX_INBOUND_BYTES),
            Err(FrameError::Closed)
        ));
    }

    #[test]
    fn clean_end_of_stream_is_closed_not_error() {
        let mut c = Cursor::new(Vec::new());
        assert!(matches!(
            read_frame(&mut c, MAX_INBOUND_BYTES),
            Err(FrameError::Closed)
        ));
    }

    #[test]
    fn truncated_frame_is_an_error_not_a_clean_close() {
        // Length says 10 bytes, only 3 follow. Must not look like a clean shutdown.
        let buf = vec![10, 0, 0, 0, b'a', b'b', b'c'];
        let err = read_frame(&mut Cursor::new(buf), MAX_INBOUND_BYTES).unwrap_err();
        assert!(matches!(err, FrameError::Io(_)), "got {err:?}");
    }

    #[test]
    fn rejects_oversized_declared_length_without_allocating() {
        // A corrupt prefix claiming 4 GB must be rejected on the length check alone.
        let buf = vec![0xFF, 0xFF, 0xFF, 0xFF];
        assert!(matches!(
            read_frame(&mut Cursor::new(buf), MAX_INBOUND_BYTES),
            Err(FrameError::TooLarge { .. })
        ));
    }

    #[test]
    fn rejects_oversized_write() {
        let big = "x".repeat(64);
        assert!(matches!(
            write_frame(&mut Vec::new(), &big, 32),
            Err(FrameError::TooLarge { .. })
        ));
    }

    #[test]
    fn rejects_invalid_utf8() {
        let buf = vec![2, 0, 0, 0, 0xFF, 0xFE];
        assert!(matches!(
            read_frame(&mut Cursor::new(buf), MAX_INBOUND_BYTES),
            Err(FrameError::NotUtf8)
        ));
    }

    #[test]
    fn rejects_zero_length_frame() {
        let buf = vec![0, 0, 0, 0];
        assert!(read_frame(&mut Cursor::new(buf), MAX_INBOUND_BYTES).is_err());
    }
}
