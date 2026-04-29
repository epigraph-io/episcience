//! Small math helpers shared across synthesis stages.

/// Cosine similarity of two equal-length `f32` vectors as `f64`.
///
/// Returns `0.0` for any of: empty inputs, mismatched lengths, or one/both
/// zero vectors. The zero-vector case is treated conservatively (rather than
/// `NaN` from `0/0`) because Stage 2's relevance prune calls this for every
/// candidate neighbour: returning `NaN` would silently propagate through the
/// `< prune` comparison in subtle ways.
pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        let xf = *x as f64;
        let yf = *y as f64;
        dot += xf * yf;
        norm_a += xf * xf;
        norm_b += yf * yf;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_orthogonal_zero() {
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-9);
    }

    #[test]
    fn cosine_identical_one() {
        assert!((cosine(&[1.0, 2.0, 3.0], &[1.0, 2.0, 3.0]) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn cosine_opposite_negone() {
        assert!((cosine(&[1.0, 2.0], &[-1.0, -2.0]) - (-1.0)).abs() < 1e-9);
    }

    #[test]
    fn cosine_zero_vec_returns_zero() {
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn cosine_mismatched_lengths_returns_zero() {
        assert_eq!(cosine(&[1.0, 2.0], &[1.0, 2.0, 3.0]), 0.0);
    }

    #[test]
    fn cosine_empty_returns_zero() {
        assert_eq!(cosine(&[], &[1.0]), 0.0);
        assert_eq!(cosine(&[1.0], &[]), 0.0);
    }
}
