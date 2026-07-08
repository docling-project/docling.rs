//! Small, dependency-free vector maths used by embedders and stores.

/// Dot product of two equal-length slices (unchecked length; extra tail ignored).
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Euclidean (L2) norm.
pub fn norm(a: &[f32]) -> f32 {
    dot(a, a).sqrt()
}

/// Cosine similarity in `[-1, 1]`; returns 0 if either vector is zero-length.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let na = norm(a);
    let nb = norm(b);
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot(a, b) / (na * nb)
    }
}

/// L2-normalize a vector in place. A zero vector is left unchanged.
pub fn normalize(v: &mut [f32]) {
    let n = norm(v);
    if n > 0.0 {
        for x in v.iter_mut() {
            *x /= n;
        }
    }
}

/// Encode a vector as a little-endian `f32` byte blob (for DB storage).
pub fn to_bytes(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

/// Decode a little-endian `f32` byte blob back into a vector.
pub fn from_bytes(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_basics() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn normalize_makes_unit_length() {
        let mut v = vec![3.0, 4.0];
        normalize(&mut v);
        assert!((norm(&v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn bytes_roundtrip() {
        let v = vec![1.5f32, -2.25, 0.0, 1024.5];
        assert_eq!(from_bytes(&to_bytes(&v)), v);
    }
}
