//! CRC-32/ISO-HDLC (the zlib/PNG CRC): reflected polynomial `0xEDB88320`,
//! initial value and final xor `0xFFFFFFFF`, check value `0xCBF43926` for
//! `"123456789"`.
//!
//! `crc32(seed, bytes)` continues a previous result, so
//! `crc32(crc32(s, a), b) == crc32(s, a ++ b)`; the log chains its records
//! this way.

const TABLE: [u32; 256] = make_table();

// Indexes are loop counters below 256 into a 256-entry array.
#[allow(clippy::indexing_slicing)]
const fn make_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 == 1 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[n] = c;
        n += 1;
    }
    table
}

pub(crate) fn crc32(seed: u32, bytes: &[u8]) -> u32 {
    let mut reg = !seed;
    for &byte in bytes {
        let slot = usize::from((reg as u8) ^ byte);
        reg = TABLE.get(slot).copied().unwrap_or(0) ^ (reg >> 8);
    }
    !reg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_answer() {
        assert_eq!(crc32(0, b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(0, b""), 0);
    }

    #[test]
    fn continuation_equals_concatenation() {
        let (a, b) = (b"write-ahead ".as_slice(), b"log".as_slice());
        assert_eq!(crc32(crc32(7, a), b), crc32(7, b"write-ahead log"));
    }
}
