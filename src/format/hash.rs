//! XXH64 (SPEC §5, D0008): the reference algorithm, hand-rolled per D0007/D0004. Bloom skip
//! structures hash with this, so it must never change once shipped — unlike `std::hash`.

const P1: u64 = 0x9E37_79B1_85EB_CA87;
const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const P3: u64 = 0x1656_67B1_9E37_79F9;
const P4: u64 = 0x85EB_CA77_C2B2_AE63;
const P5: u64 = 0x27D4_EB2F_1656_67C5;

fn round(acc: u64, input: u64) -> u64 {
    acc.wrapping_add(input.wrapping_mul(P2))
        .rotate_left(31)
        .wrapping_mul(P1)
}

fn merge_round(acc: u64, val: u64) -> u64 {
    (acc ^ round(0, val)).wrapping_mul(P1).wrapping_add(P4)
}

fn le64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}

fn le32(b: &[u8]) -> u64 {
    u32::from_le_bytes(b[..4].try_into().unwrap()) as u64
}

/// The reference XXH64: a 4-lane 32-byte stripe loop over full stripes, then 8-, 4- and
/// 1-byte tails, then the avalanche finalizer. Stable forever (blooms on disk depend on it).
pub(crate) fn xxh64(bytes: &[u8], seed: u64) -> u64 {
    let len = bytes.len();
    let mut i = 0usize;
    let mut h64 = if len >= 32 {
        let mut v1 = seed.wrapping_add(P1).wrapping_add(P2);
        let mut v2 = seed.wrapping_add(P2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(P1);
        while i + 32 <= len {
            v1 = round(v1, le64(&bytes[i..]));
            v2 = round(v2, le64(&bytes[i + 8..]));
            v3 = round(v3, le64(&bytes[i + 16..]));
            v4 = round(v4, le64(&bytes[i + 24..]));
            i += 32;
        }
        let mut h = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
        h = merge_round(h, v1);
        h = merge_round(h, v2);
        h = merge_round(h, v3);
        h = merge_round(h, v4);
        h
    } else {
        seed.wrapping_add(P5)
    };
    h64 = h64.wrapping_add(len as u64);

    while i + 8 <= len {
        let k1 = round(0, le64(&bytes[i..]));
        h64 = (h64 ^ k1).rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
        i += 8;
    }
    if i + 4 <= len {
        h64 = (h64 ^ le32(&bytes[i..]).wrapping_mul(P1))
            .rotate_left(23)
            .wrapping_mul(P2)
            .wrapping_add(P3);
        i += 4;
    }
    while i < len {
        h64 = (h64 ^ (bytes[i] as u64).wrapping_mul(P5))
            .rotate_left(11)
            .wrapping_mul(P1);
        i += 1;
    }

    h64 ^= h64 >> 33;
    h64 = h64.wrapping_mul(P2);
    h64 ^= h64 >> 29;
    h64 = h64.wrapping_mul(P3);
    h64 ^= h64 >> 32;
    h64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_answers() {
        assert_eq!(xxh64(b"", 0), 0xEF46_DB37_51D8_E999);
        assert_eq!(xxh64(b"abc", 0), 0x44BC_2CF5_AD77_0999);
    }

    #[test]
    fn stripe_loop_is_exercised_by_a_32_byte_input() {
        let data: Vec<u8> = (0..32u8).collect();
        assert_eq!(xxh64(&data, 0), 0xCBF5_9C51_16FF_32B4);
        let data40: Vec<u8> = (0..40u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(xxh64(&data40, 1), 0x421D_36A2_FFE6_CA63);
    }
}
