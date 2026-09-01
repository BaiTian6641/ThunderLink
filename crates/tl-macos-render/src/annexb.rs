//! Annex B bytestream splitting (SPEC §5).
//!
//! HEVC/H.264 on the wire are Annex B: NAL units delimited by start codes
//! (`00 00 01` or `00 00 00 01`). Semantics of [`split`]:
//!
//! - Bytes before the first start code are ignored (leading garbage).
//! - A NAL payload runs from just after a start code up to the next start
//!   code (or EOF). Bytes with no preceding start code are therefore part of
//!   the previous NAL; only garbage *before the first* start code is dropped.
//! - A single zero byte immediately preceding a start code is treated as
//!   belonging to a 4-byte start code (or `trailing_zero_8bits`) and is
//!   excluded from the previous NAL.
//! - Empty payloads (adjacent start codes) are skipped.

/// Split an Annex B bytestream into NAL unit payloads (start codes excluded).
pub(crate) fn split(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut nal_start: Option<usize> = None;
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(start) = nal_start.take() {
                let mut end = i;
                // One zero byte directly before a start code belongs to the
                // delimiter (4-byte form) or is trailing_zero_8bits.
                if end > start && data[end - 1] == 0 {
                    end -= 1;
                }
                if end > start {
                    out.push(&data[start..end]);
                }
            }
            nal_start = Some(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    if let Some(start) = nal_start {
        if data.len() > start {
            out.push(&data[start..]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::split;

    #[test]
    fn three_byte_start_codes() {
        let data = [0, 0, 1, 0x67, 0x11, 0, 0, 1, 0x68, 0x22, 0x33];
        let nals = split(&data);
        assert_eq!(nals, vec![&[0x67, 0x11][..], &[0x68, 0x22, 0x33][..]]);
    }

    #[test]
    fn four_byte_start_codes() {
        let data = [0, 0, 0, 1, 0x67, 0x11, 0, 0, 0, 1, 0x68];
        let nals = split(&data);
        assert_eq!(nals, vec![&[0x67, 0x11][..], &[0x68][..]]);
    }

    #[test]
    fn mixed_start_codes() {
        let data = [0, 0, 0, 1, 0x40, 1, 0, 0, 1, 0x42, 2, 0, 0, 0, 1, 0x44];
        let nals = split(&data);
        assert_eq!(nals, vec![&[0x40, 1][..], &[0x42, 2][..], &[0x44][..]]);
    }

    #[test]
    fn leading_garbage_is_skipped() {
        let data = [0xFF, 0xAA, 0, 0, 1, 0x67];
        assert_eq!(split(&data), vec![&[0x67][..]]);
    }

    #[test]
    fn trailing_garbage_joins_last_nal() {
        // Bytes after the final start code's NAL with no new start code are
        // indistinguishable from NAL payload, so they join the last NAL.
        let data = [0, 0, 1, 0x67, 0x11, 0x09, 0xF0];
        assert_eq!(split(&data), vec![&[0x67, 0x11, 0x09, 0xF0][..]]);
    }

    #[test]
    fn trailing_zero_before_start_code_excluded() {
        let data = [0, 0, 1, 0x67, 0, 0, 0, 1, 0x68];
        assert_eq!(split(&data), vec![&[0x67][..], &[0x68][..]]);
    }

    #[test]
    fn empty_and_garbage_only() {
        assert!(split(&[]).is_empty());
        assert!(split(&[0, 0, 0]).is_empty());
        assert!(split(&[0x11, 0x22, 0x33, 0x44]).is_empty());
        // Start code with no payload.
        assert!(split(&[0, 0, 1]).is_empty());
        assert!(split(&[0, 0, 0, 1]).is_empty());
    }

    #[test]
    fn adjacent_start_codes_skip_empty_nals() {
        let data = [0, 0, 1, 0, 0, 1, 0x67];
        assert_eq!(split(&data), vec![&[0x67][..]]);
    }

    #[test]
    fn realistic_hevc_idr_prefix() {
        // VPS(0x40..) SPS(0x42..) PPS(0x44..) IDR(0x26..) with 4-byte codes.
        let mut data = Vec::new();
        for nal in [&[0x40u8, 1, 2][..], &[0x42, 1, 2, 3][..], &[0x44, 9][..], &[0x26, 1, 0xAF][..]] {
            data.extend_from_slice(&[0, 0, 0, 1]);
            data.extend_from_slice(nal);
        }
        let nals = split(&data);
        assert_eq!(nals.len(), 4);
        assert_eq!(nals[0][0], 0x40);
        assert_eq!(nals[3], &[0x26, 1, 0xAF][..]);
    }
}
