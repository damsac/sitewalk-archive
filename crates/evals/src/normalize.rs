//! Deterministic text normalization + set similarity for the grader. No
//! network, no randomness: same input → same output, every run. This is what
//! makes eval scores comparable across prompt variants.

use std::collections::BTreeSet;

/// A tiny, closed stopword list — words that carry no extraction signal and
/// only add noise to overlap. Kept small and fixed on purpose: a big list would
/// swallow real content ("no", "not"). Do NOT tune this per-corpus.
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "to", "of", "for", "and", "or", "is", "are", "was", "were",
    "on", "in", "at", "we", "i", "it", "that", "this", "with", "need", "needs",
];

/// Lowercase, split on non-alphanumerics, drop stopwords, strip a trailing
/// plural `s`, and collect into a set. Returns a `BTreeSet` for deterministic
/// iteration order (matters only for debug output; scores are set ops).
pub fn token_set(s: &str) -> BTreeSet<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .filter(|w| !STOPWORDS.contains(w))
        .map(strip_plural)
        .collect()
}

/// Strip a single trailing plural `s` (joists→joist), but not for 2-char words
/// (as→a would be wrong) or double-s (loss→los would be wrong).
fn strip_plural(w: &str) -> String {
    if w.len() > 3 && w.ends_with('s') && !w.ends_with("ss") {
        w[..w.len() - 1].to_string()
    } else {
        w.to_string()
    }
}

/// Dice coefficient: `2·|A∩B| / (|A|+|B|)`. Symmetric, order-independent, in
/// `[0,1]`. Empty-vs-anything is 0.0 (never NaN).
pub fn dice(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    let total = a.len() + b.len();
    if total == 0 {
        return 0.0;
    }
    let inter = a.intersection(b).count();
    (2.0 * inter as f64) / total as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_lowercases_strips_punct_and_stopwords() {
        // "Order the lumber!" -> {order, lumber}  (the/for/a dropped, "!" gone)
        let t = token_set("Order the lumber!");
        assert!(t.contains("order"));
        assert!(t.contains("lumber"));
        assert!(!t.contains("the"));
    }

    #[test]
    fn normalize_strips_trailing_plural_s() {
        assert_eq!(token_set("joists"), token_set("joist"));
    }

    #[test]
    fn dice_is_one_for_identical_sets() {
        assert_eq!(dice(&token_set("order lumber"), &token_set("order lumber")), 1.0);
    }

    #[test]
    fn dice_is_order_independent() {
        let a = dice(&token_set("order the lumber"), &token_set("lumber order"));
        assert_eq!(a, 1.0, "stopword-stripped token SETS are equal regardless of order");
    }

    #[test]
    fn dice_is_zero_for_disjoint_sets() {
        assert_eq!(dice(&token_set("order lumber"), &token_set("call framer")), 0.0);
    }

    #[test]
    fn dice_partial_overlap_is_between() {
        // {order,lumber,deck} vs {order,lumber} -> 2*2/(3+2) = 0.8
        let d = dice(&token_set("order lumber deck"), &token_set("order lumber"));
        assert!((d - 0.8).abs() < 1e-9, "got {d}");
    }

    #[test]
    fn empty_sets_score_zero_not_nan() {
        assert_eq!(dice(&token_set(""), &token_set("")), 0.0);
        assert_eq!(dice(&token_set("order"), &token_set("")), 0.0);
    }
}
