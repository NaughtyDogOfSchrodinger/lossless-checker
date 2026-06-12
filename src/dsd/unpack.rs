//! Bit unpacking: expand packed 1-bit DSD bytes into ±1.0 samples.
//!
//! The hot path (one DSD64 file is ~2.8M samples/s/channel) uses a 256-entry lookup table per bit
//! order: each byte value maps directly to its 8 ±1 samples, so the inner loop is a slice copy
//! instead of eight shift-and-test operations. The tables are built at compile time.

use crate::dsd::BitOrder;

/// Build the byte→samples table. `msb` selects MSB-first (bit7 earliest) vs LSB-first (bit0).
const fn build_table(msb: bool) -> [[f32; 8]; 256] {
    let mut table = [[0.0f32; 8]; 256];
    let mut b = 0usize;
    while b < 256 {
        let mut i = 0usize;
        while i < 8 {
            let shift = if msb { 7 - i } else { i };
            table[b][i] = if (b >> shift) & 1 == 1 { 1.0 } else { -1.0 };
            i += 1;
        }
        b += 1;
    }
    table
}

static LSB_TABLE: [[f32; 8]; 256] = build_table(false);
static MSB_TABLE: [[f32; 8]; 256] = build_table(true);

/// Expand `bytes` into ±1.0 samples (appended to `out`), honoring `order`.
///
/// LSB-first (DSF): bit0 is the earliest sample. MSB-first (DFF): bit7 is the earliest.
/// `out` is caller-owned scratch so it can be reused across blocks without reallocating.
#[inline]
pub fn unpack_block(bytes: &[u8], order: BitOrder, out: &mut Vec<f32>) {
    out.clear();
    out.reserve(bytes.len() * 8);
    let table = match order {
        BitOrder::Lsb => &LSB_TABLE,
        BitOrder::Msb => &MSB_TABLE,
    };
    for &b in bytes {
        out.extend_from_slice(&table[b as usize]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsb_first_byte_one_sets_only_earliest_sample() {
        let mut out = Vec::new();
        unpack_block(&[0b0000_0001], BitOrder::Lsb, &mut out);
        assert_eq!(out.len(), 8);
        assert_eq!(out[0], 1.0); // bit0 = earliest
        assert!(out[1..].iter().all(|&s| s == -1.0));
    }

    #[test]
    fn msb_first_is_the_mirror_of_lsb_first() {
        let mut lsb = Vec::new();
        let mut msb = Vec::new();
        unpack_block(&[0b0000_0001], BitOrder::Lsb, &mut lsb);
        unpack_block(&[0b0000_0001], BitOrder::Msb, &mut msb);
        // Same byte read in opposite bit order => reversed sample sequence.
        let mut rev = msb.clone();
        rev.reverse();
        assert_eq!(lsb, rev);
        assert_eq!(msb[7], 1.0); // bit0 lands last under MSB-first
    }

    /// The lookup tables must agree with a straightforward shift-and-test for every byte value.
    #[test]
    fn tables_match_reference_for_all_bytes() {
        for b in 0u16..256 {
            let byte = b as u8;
            let mut lsb = Vec::new();
            let mut msb = Vec::new();
            unpack_block(&[byte], BitOrder::Lsb, &mut lsb);
            unpack_block(&[byte], BitOrder::Msb, &mut msb);
            for i in 0..8 {
                let want_lsb = if (byte >> i) & 1 == 1 { 1.0 } else { -1.0 };
                let want_msb = if (byte >> (7 - i)) & 1 == 1 { 1.0 } else { -1.0 };
                assert_eq!(lsb[i], want_lsb, "LSB byte {byte} bit {i}");
                assert_eq!(msb[i], want_msb, "MSB byte {byte} bit {i}");
            }
        }
    }
}
