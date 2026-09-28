//! Panic-free little-endian byte-cursor helpers shared by the wire codecs.
//!
//! Wire bytes are untrusted input (`rules/global_rules.md`, Production Panic
//! Policy). Every helper here reads through `slice::get` with a checked end
//! offset and converts the slot with `<[u8; N]>::try_from`, so a short or
//! malformed buffer surfaces as [`WireError::Truncated`] instead of a panic.
//! There is no `copy_from_slice`, no raw `offset + N` and no indexing.

use super::error::WireError;

/// Reads the `N`-byte array starting at `offset`.
///
/// # Errors
///
/// Returns [`WireError::Truncated`] when `offset + N` overflows `usize` or
/// runs past the end of `buf`.
#[inline]
pub(crate) fn read_array<const N: usize>(buf: &[u8], offset: usize) -> Result<[u8; N], WireError> {
    let end = offset.checked_add(N).ok_or(WireError::Truncated)?;
    let slot = buf.get(offset..end).ok_or(WireError::Truncated)?;
    <[u8; N]>::try_from(slot).map_err(|_| WireError::Truncated)
}

/// Reads a little-endian `u64` at `offset`.
///
/// # Errors
///
/// Same as [`read_array`].
#[inline]
pub(crate) fn read_u64_le(buf: &[u8], offset: usize) -> Result<u64, WireError> {
    read_array::<8>(buf, offset).map(u64::from_le_bytes)
}

/// Reads a little-endian `i64` at `offset`.
///
/// # Errors
///
/// Same as [`read_array`].
#[inline]
pub(crate) fn read_i64_le(buf: &[u8], offset: usize) -> Result<i64, WireError> {
    read_array::<8>(buf, offset).map(i64::from_le_bytes)
}

/// Reads a little-endian `u32` at `offset`.
///
/// # Errors
///
/// Same as [`read_array`].
#[inline]
pub(crate) fn read_u32_le(buf: &[u8], offset: usize) -> Result<u32, WireError> {
    read_array::<4>(buf, offset).map(u32::from_le_bytes)
}

/// Reads a little-endian `u16` at `offset`.
///
/// # Errors
///
/// Same as [`read_array`].
#[inline]
pub(crate) fn read_u16_le(buf: &[u8], offset: usize) -> Result<u16, WireError> {
    read_array::<2>(buf, offset).map(u16::from_le_bytes)
}

/// Reads the single byte at `offset`.
///
/// # Errors
///
/// Returns [`WireError::Truncated`] when `offset` is out of bounds.
#[inline]
pub(crate) fn read_u8(buf: &[u8], offset: usize) -> Result<u8, WireError> {
    buf.get(offset).copied().ok_or(WireError::Truncated)
}

/// Reserves room for `additional` more bytes in `out` without panicking.
///
/// After a successful call, appending up to `additional` bytes with
/// `extend_from_slice` / `push` cannot reallocate, so it cannot hit the
/// capacity-overflow panic in `Vec`'s growth path.
///
/// # Errors
///
/// Returns [`WireError::CapacityOverflow`] when the new capacity would
/// exceed `isize::MAX` bytes or the allocator reports a failure.
#[inline]
pub(crate) fn reserve_payload(out: &mut Vec<u8>, additional: usize) -> Result<(), WireError> {
    out.try_reserve(additional)
        .map_err(|_| WireError::CapacityOverflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_in_bounds() {
        let buf = [1u8, 0, 0, 0, 0, 0, 0, 0, 0xFE, 0xFF];
        assert_eq!(read_u64_le(&buf, 0), Ok(1));
        assert_eq!(read_u16_le(&buf, 8), Ok(0xFFFE));
        assert_eq!(read_u32_le(&buf, 0), Ok(1));
        assert_eq!(read_u8(&buf, 9), Ok(0xFF));
        assert_eq!(read_i64_le(&[0xFF; 8], 0), Ok(-1));
    }

    #[test]
    fn short_buffer_is_truncated() {
        let buf = [0u8; 7];
        assert_eq!(read_u64_le(&buf, 0), Err(WireError::Truncated));
        assert_eq!(read_i64_le(&buf, 0), Err(WireError::Truncated));
        assert_eq!(read_u16_le(&buf, 6), Err(WireError::Truncated));
        assert_eq!(read_u8(&buf, 7), Err(WireError::Truncated));
    }

    #[test]
    fn offset_overflow_is_truncated() {
        let buf = [0u8; 8];
        assert_eq!(read_u64_le(&buf, usize::MAX), Err(WireError::Truncated));
        assert_eq!(read_u16_le(&buf, usize::MAX - 1), Err(WireError::Truncated));
    }

    #[test]
    fn reserve_payload_rejects_impossible_capacity() {
        let mut out = vec![0u8; 1];
        // `len + usize::MAX` overflows: the typed error replaces the
        // `Vec::reserve` "capacity overflow" panic.
        assert_eq!(
            reserve_payload(&mut out, usize::MAX),
            Err(WireError::CapacityOverflow)
        );
        // The buffer is left untouched.
        assert_eq!(out, vec![0u8]);
    }

    #[test]
    fn reserve_payload_grows_exactly_enough() {
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(reserve_payload(&mut out, 44), Ok(()));
        assert!(out.capacity() >= 44);
    }
}
