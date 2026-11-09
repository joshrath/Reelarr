//! 16-bit Galois field arithmetic for PAR2 2.0 recovery.
//!
//! The specification uses GF(2^16) with generator polynomial 0x0001100B
//! (x^16 + x^12 + x^3 + x + 1) and assigns each input slice a constant equal
//! to a power of two whose exponent is not 0 mod 3, 5, 17 or 257, so the
//! constant has order 65535 and recovery matrices stay invertible.

const POLY: u32 = 0x1_100B;
const ORDER: u32 = 65_535;

/// Log/exp tables for GF(2^16) with the PAR2 generator.
pub struct Field {
    exp: [u16; ORDER as usize],
    log: [u16; 65_536],
}

impl Field {
    /// Build the log and exp tables.
    pub fn new() -> Self {
        let mut exp = [0u16; ORDER as usize];
        let mut log = [0u16; 65_536];
        let mut x: u32 = 1;
        for i in 0..ORDER {
            exp[i as usize] = x as u16;
            log[x as usize] = i as u16;
            x <<= 1;
            if x & 0x1_0000 != 0 {
                x ^= POLY;
            }
        }
        Self { exp, log }
    }

    /// Product in the field. Either factor 0 yields 0.
    pub fn mul(&self, a: u16, b: u16) -> u16 {
        if a == 0 || b == 0 {
            0
        } else {
            let sum = u32::from(self.log[a as usize]) + u32::from(self.log[b as usize]);
            self.exp[(sum % ORDER) as usize]
        }
    }

    /// `a` raised to `e`. `0^0` is 1.
    pub fn pow(&self, a: u16, e: u32) -> u16 {
        if a == 0 {
            return if e == 0 { 1 } else { 0 };
        }
        let p = (u64::from(self.log[a as usize]) * u64::from(e)) % u64::from(ORDER);
        self.exp[p as usize]
    }

    /// Multiplicative inverse, or `None` for 0.
    pub fn inv(&self, a: u16) -> Option<u16> {
        if a == 0 {
            None
        } else {
            Some(self.exp[((ORDER - u32::from(self.log[a as usize])) % ORDER) as usize])
        }
    }
}

impl Default for Field {
    fn default() -> Self {
        Self::new()
    }
}

/// Input-slice constants in the order the specification assigns them.
///
/// There are 32768 valid constants. The first few, given by the spec, are
/// 2, 4, 16, 128, 256, 2048, 8192, 16384, 4107, 32856, 17132.
pub fn slice_constants(field: &Field) -> Vec<u16> {
    let mut out = Vec::with_capacity(32_768);
    for n in 0..ORDER {
        if n % 3 != 0 && n % 5 != 0 && n % 17 != 0 && n % 257 != 0 {
            out.push(field.pow(2, n));
        }
    }
    out
}

/// Invert an `n × n` matrix over the field (Gauss–Jordan). `None` if singular.
pub fn invert(field: &Field, matrix: &[Vec<u16>]) -> Option<Vec<Vec<u16>>> {
    let n = matrix.len();
    if n == 0 || matrix.iter().any(|row| row.len() != n) {
        return None;
    }
    let mut m: Vec<Vec<u16>> = matrix
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let mut wide = row.clone();
            wide.extend((0..n).map(|j| u16::from(i == j)));
            wide
        })
        .collect();
    for col in 0..n {
        let pivot = (col..n).find(|&row| m[row][col] != 0)?;
        m.swap(col, pivot);
        let inv = field.inv(m[col][col])?;
        for x in &mut m[col] {
            *x = field.mul(*x, inv);
        }
        for row in 0..n {
            if row == col {
                continue;
            }
            let factor = m[row][col];
            if factor == 0 {
                continue;
            }
            let src = m[col].clone();
            for (dest, s) in m[row].iter_mut().zip(src) {
                *dest ^= field.mul(factor, s);
            }
        }
    }
    Some(m.iter().map(|row| row[n..].to_vec()).collect())
}

/// Column vector `matrix * rhs`.
pub fn mul_vec(field: &Field, matrix: &[Vec<u16>], rhs: &[u16]) -> Vec<u16> {
    matrix
        .iter()
        .map(|row| {
            row.iter()
                .zip(rhs)
                .fold(0u16, |acc, (&a, &b)| acc ^ field.mul(a, b))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_lists_the_first_constants() {
        let field = Field::new();
        let consts = slice_constants(&field);
        assert_eq!(consts.len(), 32_768);
        assert_eq!(
            &consts[..11],
            &[2, 4, 16, 128, 256, 2048, 8192, 16384, 4107, 32856, 17132]
        );
    }

    #[test]
    fn mul_is_commutative_and_has_identity() {
        let field = Field::new();
        for a in [0u16, 1, 2, 3, 0x110B, 0xFFFF, 4107] {
            assert_eq!(field.mul(a, 1), a);
            assert_eq!(field.mul(a, 0), 0);
            for b in [0u16, 1, 7, 99, 0x8000] {
                assert_eq!(field.mul(a, b), field.mul(b, a));
            }
        }
    }

    #[test]
    fn inv_round_trips() {
        let field = Field::new();
        assert!(field.inv(0).is_none());
        for a in 1u16..=200 {
            let inv = field.inv(a).unwrap();
            assert_eq!(field.mul(a, inv), 1);
        }
        assert_eq!(field.mul(0xFFFF, field.inv(0xFFFF).unwrap()), 1);
    }

    #[test]
    fn invert_identity_and_small_system() {
        let field = Field::new();
        let id = vec![vec![1, 0], vec![0, 1]];
        assert_eq!(invert(&field, &id), Some(id));

        // A = [[2, 4], [16, 128]]; solve A x = [1, 1].
        let a = vec![vec![2, 4], vec![16, 128]];
        let inv = invert(&field, &a).unwrap();
        let prod = (0..2)
            .map(|i| {
                (0..2)
                    .map(|j| (0..2).fold(0u16, |acc, k| acc ^ field.mul(a[i][k], inv[k][j])))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        assert_eq!(prod, vec![vec![1, 0], vec![0, 1]]);
    }
}
