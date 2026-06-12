//! Bit unpacking: expand packed 1-bit DSD bytes into ±1.0 samples.

use crate::dsd::BitOrder;

/// Expand `bytes` into ±1.0 samples (appended to `out`), honoring `order`.
///
/// LSB-first (DSF): bit0 is the earliest sample. MSB-first (DFF): bit7 is the earliest.
/// `out` is caller-owned scratch so it can be reused across blocks without reallocating.
#[inline]
pub fn unpack_block(bytes: &[u8], order: BitOrder, out: &mut Vec<f32>) {
    out.clear();
    out.reserve(bytes.len() * 8);
    match order {
        BitOrder::Lsb => {
            for &b in bytes {
                for i in 0..8 {
                    out.push(if (b >> i) & 1 == 1 { 1.0 } else { -1.0 });
                }
            }
        }
        BitOrder::Msb => {
            for &b in bytes {
                for i in (0..8).rev() {
                    out.push(if (b >> i) & 1 == 1 { 1.0 } else { -1.0 });
                }
            }
        }
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
}
