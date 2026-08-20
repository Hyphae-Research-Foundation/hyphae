// SPDX-License-Identifier: Apache-2.0

//! CRC-64 (Jones polynomial, reflected) as used by the RDB file trailer.

/// Reflected CRC-64/Jones polynomial.
const POLYNOMIAL: u64 = 0x95ac_9329_ac4b_c9b5;

const fn build_table() -> [u64; 256] {
    let mut table = [0_u64; 256];
    let mut index = 0_usize;
    while index < 256 {
        let mut crc = index as u64;
        let mut bit = 0_u8;
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

const TABLE: [u64; 256] = build_table();

/// Folds `bytes` into a running CRC-64/Jones value.
pub(crate) fn update(crc: u64, bytes: &[u8]) -> u64 {
    let mut value = crc;
    for byte in bytes {
        let low = u8::try_from(value & 0xff).unwrap_or(0);
        value = TABLE[usize::from(low ^ byte)] ^ (value >> 8);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::update;

    #[test]
    fn known_answer_matches_the_rdb_reference() {
        assert_eq!(update(0, b"123456789"), 0xe9c6_d914_c4b8_d9ca);
    }

    #[test]
    fn update_is_incremental() {
        let whole = update(0, b"hyphae external migration");
        let split = update(update(0, b"hyphae external"), b" migration");
        assert_eq!(whole, split);
    }
}
