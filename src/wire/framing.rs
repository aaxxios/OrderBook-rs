//! Length-prefixed binary frame codec.
//!
//! Every frame on the wire has the layout:
//!
//! ```text
//! [len: u32 LE][kind: u8][payload: …]
//! ```
//!
//! `len` is the byte length of `kind + payload` — it does NOT include the
//! `len` field itself. All multi-byte integers on the wire are little-endian.
//!
//! Framing is symmetric for inbound and outbound traffic; the only thing that
//! differs is which `kind` discriminants are valid in each direction (see
//! [`super::MessageKind`]).

use super::bytes::{read_u8, read_u32_le};
use super::error::WireError;
use std::io::{self, Write};

/// Size in bytes of the length prefix.
const LEN_PREFIX: usize = 4;
/// Size in bytes of the kind byte.
const KIND_SIZE: usize = 1;
/// Minimum frame size: a `len` prefix plus a single `kind` byte (zero-byte
/// payload). Also the offset of the first payload byte.
const MIN_FRAME_SIZE: usize = LEN_PREFIX + KIND_SIZE;

/// Encodes a frame into `out`.
///
/// Writes `len` (4 bytes, little-endian, value `1 + payload.len()`), the
/// `kind` byte, and the payload, in that order.
///
/// # Errors
///
/// Propagates any [`io::Error`] returned by the underlying writer, and
/// returns [`io::ErrorKind::InvalidInput`] when `kind + payload` does not
/// fit in the wire-format `u32` length prefix — guarantees the declared
/// frame length always matches the bytes written.
#[inline]
pub fn encode_frame<W: Write>(kind: u8, payload: &[u8], out: &mut W) -> io::Result<()> {
    // `len` is the size of `kind + payload`. Reject payloads whose encoded
    // body length cannot be represented in the wire-format `u32` prefix so we
    // never emit a frame whose declared length disagrees with the bytes
    // written.
    let body_len_usize = payload
        .len()
        .checked_add(KIND_SIZE)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "frame payload too large"))?;
    let body_len = u32::try_from(body_len_usize)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame payload too large"))?;
    out.write_all(&body_len.to_le_bytes())?;
    out.write_all(&[kind])?;
    out.write_all(payload)?;
    Ok(())
}

/// Decodes a single frame from the start of `buf`.
///
/// On success returns `(kind, payload, bytes_consumed)`. `bytes_consumed`
/// includes the `len` prefix and the `kind` byte, so callers can advance
/// their read cursor by exactly that many bytes.
///
/// # Errors
///
/// Returns [`WireError::Truncated`] if `buf` is shorter than the framing
/// header or shorter than the body length declared by the header.
#[inline]
#[must_use = "the decoded value (or error) must be handled"]
pub fn decode_frame(buf: &[u8]) -> Result<(u8, &[u8], usize), WireError> {
    if buf.len() < MIN_FRAME_SIZE {
        return Err(WireError::Truncated);
    }
    // Wire bytes are untrusted: every read below goes through a checked
    // offset and `slice::get`, never indexing, `copy_from_slice` or raw
    // offset arithmetic (Production Panic Policy).
    let body_len = usize::try_from(read_u32_le(buf, 0)?)
        .map_err(|_| WireError::InvalidPayload("frame length exceeds usize"))?;

    if body_len < KIND_SIZE {
        return Err(WireError::InvalidPayload("frame body shorter than kind"));
    }

    let total = LEN_PREFIX
        .checked_add(body_len)
        .ok_or(WireError::Truncated)?;
    if buf.len() < total {
        return Err(WireError::Truncated);
    }

    let kind = read_u8(buf, LEN_PREFIX)?;
    let payload = buf.get(MIN_FRAME_SIZE..total).ok_or(WireError::Truncated)?;
    Ok((kind, payload, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_empty_payload() {
        let mut buf = Vec::new();
        encode_frame(0x01, &[], &mut buf).expect("encode empty payload");
        let (kind, payload, consumed) = decode_frame(&buf).expect("decode empty payload");
        assert_eq!(kind, 0x01);
        assert!(payload.is_empty());
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn roundtrip_with_payload() {
        let mut buf = Vec::new();
        let payload = [1u8, 2, 3, 4, 5];
        encode_frame(0x42, &payload, &mut buf).expect("encode payload");
        let (kind, decoded, consumed) = decode_frame(&buf).expect("decode payload");
        assert_eq!(kind, 0x42);
        assert_eq!(decoded, &payload);
        assert_eq!(consumed, buf.len());
    }

    #[test]
    fn truncated_header_returns_truncated() {
        // Only 3 bytes — shorter than the 5-byte minimum frame.
        let buf = [0x05, 0x00, 0x00];
        assert_eq!(decode_frame(&buf), Err(WireError::Truncated));
    }

    #[test]
    fn truncated_payload_returns_truncated() {
        // Body length declares 10 bytes but we only have the 5-byte header.
        let buf = [0x0A, 0x00, 0x00, 0x00, 0x01];
        assert_eq!(decode_frame(&buf), Err(WireError::Truncated));
    }

    #[test]
    fn zero_body_length_is_invalid() {
        // `len = 0` means there isn't even a kind byte — protocol violation.
        let buf = [0x00, 0x00, 0x00, 0x00, 0x00];
        assert!(matches!(
            decode_frame(&buf),
            Err(WireError::InvalidPayload(_))
        ));
    }

    #[test]
    fn decode_consumes_only_one_frame_at_a_time() {
        let mut buf = Vec::new();
        encode_frame(0x01, &[0xAA, 0xBB], &mut buf).expect("encode frame 1");
        encode_frame(0x02, &[0xCC], &mut buf).expect("encode frame 2");

        let (k1, p1, used1) = decode_frame(&buf).expect("decode frame 1");
        assert_eq!(k1, 0x01);
        assert_eq!(p1, &[0xAA, 0xBB]);

        let rest = buf.get(used1..).expect("rest of buffer");
        let (k2, p2, used2) = decode_frame(rest).expect("decode frame 2");
        assert_eq!(k2, 0x02);
        assert_eq!(p2, &[0xCC]);
        assert_eq!(used1 + used2, buf.len());
    }

    #[test]
    fn max_declared_length_is_truncated_not_panic() {
        // `len = u32::MAX` on a 6-byte buffer: the checked arithmetic and
        // `get` reject it as truncated.
        let buf = [0xFF, 0xFF, 0xFF, 0xFF, 0x01, 0x00];
        assert_eq!(decode_frame(&buf), Err(WireError::Truncated));
    }

    #[test]
    fn every_short_prefix_of_a_valid_frame_is_truncated() {
        let mut buf = Vec::new();
        encode_frame(0x01, &[1, 2, 3, 4], &mut buf).expect("encode frame");
        for cut in 0..buf.len() {
            let prefix = buf.get(..cut).expect("prefix");
            assert_eq!(decode_frame(prefix), Err(WireError::Truncated), "cut={cut}");
        }
    }
}
