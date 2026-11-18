//! Software CRC-32C (Castagnoli) used to frame every durable journal record.
//!
//! Each journal line is an envelope `{"c":"<crc32c>","r":<record>}` whose `c`
//! is the lowercase hexadecimal CRC-32C of the exact serialized `r` bytes. The
//! polynomial is the Castagnoli polynomial `0x1EDC6F41`; the reflected form used
//! by the standard byte-at-a-time table algorithm is `0x82F63B78`. This is a
//! self-contained implementation rather than a dependency: the one CRC crate
//! already in the tree (`crc32fast`) computes the IEEE polynomial, not
//! Castagnoli, and a small software table needs no review surface.

/// Reflected Castagnoli polynomial.
const POLYNOMIAL: u32 = 0x82F6_3B78;

/// Byte-at-a-time lookup table, built once at compile time.
const TABLE: [u32; 256] = build_table();

const fn build_table() -> [u32; 256] {
    let mut table = [0_u32; 256];
    let mut index = 0_usize;
    while index < 256 {
        let mut crc = index as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ POLYNOMIAL
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
}

/// Compute the CRC-32C of `bytes`.
pub(crate) fn crc32c(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
    let mut index = 0;
    while index < bytes.len() {
        let slot = ((crc ^ u32::from(bytes[index])) & 0xFF) as usize;
        crc = (crc >> 8) ^ TABLE[slot];
        index += 1;
    }
    crc ^ 0xFFFF_FFFF
}

/// Lowercase, zero-padded eight-character hexadecimal CRC-32C of `bytes`.
pub(crate) fn crc32c_hex(bytes: &[u8]) -> String {
    format!("{:08x}", crc32c(bytes))
}

#[cfg(test)]
mod tests {
    use super::{crc32c, crc32c_hex};

    /// Published CRC-32C (Castagnoli) test vectors. The single-value check
    /// (`"123456789"` -> `0xe3069283`) is the universal CRC catalogue anchor;
    /// the four 32-byte patterns are the iSCSI vectors from RFC 3720 B.4. These
    /// are external ground truth, independent of this implementation.
    #[test]
    fn matches_published_vectors() {
        assert_eq!(crc32c(b""), 0x0000_0000);
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
        assert_eq!(crc32c(&[0x00_u8; 32]), 0x8a91_36aa);
        assert_eq!(crc32c(&[0xff_u8; 32]), 0x62a8_ab43);

        let mut ascending = [0_u8; 32];
        for (value, slot) in ascending.iter_mut().enumerate() {
            *slot = value as u8;
        }
        assert_eq!(crc32c(&ascending), 0x46dd_794e);

        let mut descending = [0_u8; 32];
        for (value, slot) in descending.iter_mut().enumerate() {
            *slot = (31 - value) as u8;
        }
        assert_eq!(crc32c(&descending), 0x113f_db5c);
    }

    #[test]
    fn hex_is_lowercase_and_zero_padded() {
        assert_eq!(crc32c_hex(b""), "00000000");
        assert_eq!(crc32c_hex(b"123456789"), "e3069283");
        assert_eq!(crc32c_hex(&[0x00_u8; 32]), "8a9136aa");
    }
}
