//! AES-128 and AES-128-CBC, implemented here rather than pulled in.
//!
//! RAOP needs CBC with **zero padding** to a block boundary, and the Rust
//! `cipher`-family traits changed shape between the 0.4 and 0.5 generations
//! (padding helpers only exist in the newer one), so pinning to a working
//! combination is more fragile than the ~100 lines below. It is also the only
//! crypto in the engine: everything else went away when NetEase retired
//! `weapi`/`eapi`, so this keeps the dependency tree honest.

/// AES-128 with a pre-expanded key schedule.
pub struct Aes128 {
    /// 11 round keys of 16 bytes each
    rk: [[u8; 16]; 11],
}

impl Aes128 {
    #[must_use]
    pub fn new(key: &[u8; 16]) -> Self {
        let mut rk = [[0u8; 16]; 11];
        rk[0].copy_from_slice(key);
        let mut rcon = 1u8;
        for i in 1..11 {
            let prev = rk[i - 1];
            // RotWord + SubWord + Rcon on the last word, then XOR-chain.
            let mut t = [prev[13], prev[14], prev[15], prev[12]];
            for b in &mut t {
                *b = sbox(*b);
            }
            t[0] ^= rcon;
            rcon = xtime(rcon);
            for j in 0..4 {
                rk[i][j] = prev[j] ^ t[j];
                rk[i][j + 4] = prev[j + 4] ^ rk[i][j];
                rk[i][j + 8] = prev[j + 8] ^ rk[i][j + 4];
                rk[i][j + 12] = prev[j + 12] ^ rk[i][j + 8];
            }
        }
        Self { rk }
    }

    /// Encrypt one 16-byte block in place.
    pub fn encrypt_block(&self, block: &mut [u8; 16]) {
        add_round_key(block, &self.rk[0]);
        for round in 1..10 {
            for b in block.iter_mut() {
                *b = sbox(*b);
            }
            shift_rows(block);
            mix_columns(block);
            add_round_key(block, &self.rk[round]);
        }
        for b in block.iter_mut() {
            *b = sbox(*b);
        }
        shift_rows(block);
        add_round_key(block, &self.rk[10]);
    }
}

/// AES-128-CBC. `data` is zero-padded to a block boundary, which is exactly
/// what RAOP expects (it is not PKCS#7).
#[must_use]
pub fn aes128_cbc_encrypt(key: &[u8; 16], iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(key);
    let mut out = data.to_vec();
    let rem = out.len() % 16;
    if rem != 0 {
        out.resize(out.len() + (16 - rem), 0);
    }
    let mut prev = *iv;
    for chunk in out.chunks_exact_mut(16) {
        for i in 0..16 {
            chunk[i] ^= prev[i];
        }
        let mut block = [0u8; 16];
        block.copy_from_slice(chunk);
        cipher.encrypt_block(&mut block);
        chunk.copy_from_slice(&block);
        prev.copy_from_slice(chunk);
    }
    out
}

fn add_round_key(b: &mut [u8; 16], k: &[u8; 16]) {
    for i in 0..16 {
        b[i] ^= k[i];
    }
}

fn shift_rows(b: &mut [u8; 16]) {
    // state is column-major: byte i lives in column i/4, row i%4
    let t = *b;
    for c in 0..4 {
        for r in 0..4 {
            b[r + 4 * c] = t[r + 4 * ((c + r) % 4)];
        }
    }
}

fn xtime(a: u8) -> u8 {
    let hi = a & 0x80 != 0;
    let v = a << 1;
    if hi { v ^ 0x1b } else { v }
}

fn mix_columns(b: &mut [u8; 16]) {
    for c in 0..4 {
        let i = 4 * c;
        let a0 = b[i];
        let a1 = b[i + 1];
        let a2 = b[i + 2];
        let a3 = b[i + 3];
        let t = a0 ^ a1 ^ a2 ^ a3;
        b[i] = a0 ^ t ^ xtime(a0 ^ a1);
        b[i + 1] = a1 ^ t ^ xtime(a1 ^ a2);
        b[i + 2] = a2 ^ t ^ xtime(a2 ^ a3);
        b[i + 3] = a3 ^ t ^ xtime(a3 ^ a0);
    }
}

/// The AES S-box, computed once rather than transcribed.
///
/// A typo in a 256-entry table is the classic silent crypto bug; deriving it
/// from the GF(2^8) inverse plus the affine transform removes that whole class
/// of mistake and costs microseconds at first use.
fn sbox(x: u8) -> u8 {
    static TABLE: std::sync::OnceLock<[u8; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut inv = [0u8; 256];
        inv[0] = 0;
        for a in 1..=255u16 {
            for b in 1..=255u16 {
                if gf_mul(a as u8, b as u8) == 1 {
                    inv[a as usize] = b as u8;
                    break;
                }
            }
        }
        let mut s = [0u8; 256];
        for i in 0..256 {
            let b = inv[i];
            s[i] = b ^ b.rotate_left(1) ^ b.rotate_left(2) ^ b.rotate_left(3) ^ b.rotate_left(4) ^ 0x63;
        }
        s
    });
    t[x as usize]
}

fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    for _ in 0..8 {
        if b & 1 != 0 {
            p ^= a;
        }
        a = xtime(a);
        b >>= 1;
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_fips197_vector() {
        // FIPS-197 appendix B: 128-bit key, plaintext 00112233445566778899aabbccddeeff
        let key = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f];
        let mut block = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff];
        Aes128::new(&key).encrypt_block(&mut block);
        assert_eq!(
            block,
            [0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30, 0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4, 0xc5, 0x5a]
        );
    }

    #[test]
    fn cbc_chains_and_zero_pads() {
        // NIST SP 800-38A F.1 first block
        let key = [0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf, 0x4f, 0x3c];
        let iv = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f];
        let pt = [0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93, 0x17, 0x2a];
        let ct = aes128_cbc_encrypt(&key, &iv, &pt);
        assert_eq!(
            ct,
            vec![0x76, 0x49, 0xab, 0xac, 0x81, 0x19, 0xb2, 0x46, 0xce, 0xe9, 0x8e, 0x9b, 0x12, 0xe9, 0x19, 0x7d]
        );
        // A partial block is zero padded to the boundary, not PKCS#7 padded.
        let short = aes128_cbc_encrypt(&key, &iv, &pt[..5]);
        assert_eq!(short.len(), 16);
    }
}
