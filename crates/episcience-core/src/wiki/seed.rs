use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct SeedCandidate {
    pub id: Uuid,
    /// Similarity to the article query (higher = more on-topic).
    pub relevance: f32,
    pub embedding: Vec<f32>,
}

fn cosine(a: &[f32], b: &[f32]) -> Option<f32> {
    if a.is_empty() || a.len() != b.len() {
        return None;
    }
    let (mut d, mut na, mut nb) = (0f32, 0f32, 0f32);
    for (x, y) in a.iter().zip(b) {
        d += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return None;
    }
    Some(d / (na.sqrt() * nb.sqrt()))
}

/// Maximal-marginal-relevance selection with near-duplicate suppression.
/// Greedy: each round picks the candidate maximising
/// `lambda * relevance - (1 - lambda) * max_cos_to_selected`; a candidate whose
/// cosine to any selected one is `>= dup_cosine` is dropped (same fact restated).
/// Candidates with an unusable embedding (empty, zero, wrong length) are skipped.
/// Ties keep input order.
pub fn select_article_seeds(
    cands: &[SeedCandidate],
    budget: usize,
    dup_cosine: f32,
    lambda: f32,
) -> Vec<Uuid> {
    let dim = cands
        .iter()
        .find(|c| cosine(&c.embedding, &c.embedding).is_some())
        .map(|c| c.embedding.len());
    let mut pool: Vec<&SeedCandidate> = cands
        .iter()
        .filter(|c| Some(c.embedding.len()) == dim && cosine(&c.embedding, &c.embedding).is_some())
        .collect();
    let mut picked: Vec<&SeedCandidate> = Vec::new();
    while picked.len() < budget && !pool.is_empty() {
        let mut best: Option<(usize, f32)> = None;
        for (i, c) in pool.iter().enumerate() {
            let redundancy = picked
                .iter()
                .filter_map(|p| cosine(&c.embedding, &p.embedding))
                .fold(0f32, f32::max);
            let score = lambda * c.relevance - (1.0 - lambda) * redundancy;
            if best.map_or(true, |(_, s)| score > s) {
                best = Some((i, score));
            }
        }
        let (i, _) = best.expect("pool non-empty");
        let chosen = pool.remove(i);
        pool.retain(|c| cosine(&c.embedding, &chosen.embedding).map_or(true, |s| s < dup_cosine));
        picked.push(chosen);
    }
    picked.into_iter().map(|c| c.id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: u128, rel: f32, e: &[f32]) -> SeedCandidate {
        SeedCandidate {
            id: Uuid::from_u128(id),
            relevance: rel,
            embedding: e.to_vec(),
        }
    }

    #[test]
    fn near_duplicates_collapse_to_the_more_relevant_one() {
        let cands = [
            c(1, 0.9, &[1.0, 0.0]),
            c(2, 0.8, &[0.999, 0.01]),
            c(3, 0.5, &[0.0, 1.0]),
        ];
        let got = select_article_seeds(&cands, 3, 0.95, 0.7);
        assert_eq!(got, vec![Uuid::from_u128(1), Uuid::from_u128(3)]);
    }

    #[test]
    fn prefers_a_diverse_candidate_over_a_slightly_more_relevant_similar_one() {
        // 1 is picked first; 2 is close to 1 (cos≈0.89, below dup threshold) but 3 is orthogonal.
        let cands = [
            c(1, 0.90, &[1.0, 0.0]),
            c(2, 0.88, &[0.9, 0.45]),
            c(3, 0.80, &[0.0, 1.0]),
        ];
        let got = select_article_seeds(&cands, 2, 0.95, 0.7);
        assert_eq!(got, vec![Uuid::from_u128(1), Uuid::from_u128(3)]);
    }

    #[test]
    fn respects_budget_and_handles_empty_and_degenerate_input() {
        assert!(select_article_seeds(&[], 5, 0.95, 0.7).is_empty());
        let cands: Vec<_> = (0..10)
            .map(|i| c(i, 1.0 - i as f32 / 10.0, &[i as f32, 1.0]))
            .collect();
        assert_eq!(select_article_seeds(&cands, 3, 0.95, 0.7).len(), 3);
        // A candidate with an empty or zero embedding is skipped, never a NaN pick.
        let odd = [
            c(1, 0.9, &[]),
            c(2, 0.8, &[0.0, 0.0]),
            c(3, 0.7, &[1.0, 0.0]),
        ];
        assert_eq!(
            select_article_seeds(&odd, 3, 0.95, 0.7),
            vec![Uuid::from_u128(3)]
        );
    }

    #[test]
    fn ties_break_by_input_order_deterministically() {
        let cands = [c(5, 0.5, &[1.0, 0.0]), c(4, 0.5, &[0.0, 1.0])];
        assert_eq!(
            select_article_seeds(&cands, 1, 0.95, 0.7),
            vec![Uuid::from_u128(5)]
        );
    }
}
