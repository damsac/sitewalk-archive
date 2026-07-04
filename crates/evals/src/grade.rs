//! Deterministic grader (spec R6/R7). Grades an `Observed` (pipeline output as
//! plain data) against a scenario's `GroundTruth`. No store, no network — so
//! grader tests need no pipeline, and scores are reproducible across runs and
//! across prompt variants (the property a prompt-optimizer requires).
//!
//! Scoring encodes product values: F0.5 weights precision 2× recall (R6:
//! under-extraction is cheaper than over-extraction), and a distractor
//! false-positive rate measures chatter wrongly promoted to items.

use serde::{Deserialize, Serialize};

use crate::corpus::GroundTruth;
use crate::normalize::{dice, token_set};

/// Dice threshold for "these two texts are the same item". 0.5 = at least half
/// the combined token mass overlaps. Tuned once, fixed — moving it per-corpus
/// would make scores incomparable.
pub const MATCH_THRESHOLD: f64 = 0.5;

/// β² for F0.5 — precision weighted 2× recall (R6).
const BETA_SQ: f64 = 0.25;

/// Pipeline output as plain data (built from store rows by `run.rs`).
#[derive(Clone, Debug)]
pub struct Observed {
    pub items: Vec<ObservedItem>,
    pub contacts: Vec<ObservedContact>,
    pub summary_present: bool,
}

#[derive(Clone, Debug)]
pub struct ObservedItem {
    pub kind: String,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct ObservedContact {
    pub name: String,
    pub trade: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PrecisionRecall {
    pub true_positives: usize,
    pub false_positives: usize,
    pub false_negatives: usize,
    pub precision: f64,
    pub recall: f64,
    pub f1: f64,
}

impl PrecisionRecall {
    fn from_counts(tp: usize, fp: usize, fn_: usize) -> Self {
        let precision = ratio(tp, tp + fp);
        let recall = ratio(tp, tp + fn_);
        let f1 = harmonic(precision, recall, 1.0);
        PrecisionRecall { true_positives: tp, false_positives: fp, false_negatives: fn_, precision, recall, f1 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct KindScore {
    pub kind: String,
    pub true_positives: usize,
    pub false_positives: usize,
    pub false_negatives: usize,
    pub precision: f64,
    pub recall: f64,
    pub f1: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ConfusionEntry {
    pub expected_kind: String,
    pub produced_kind: String,
    pub count: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScenarioScore {
    pub overall: PrecisionRecall,
    pub per_kind: Vec<KindScore>,
    pub confusion: Vec<ConfusionEntry>,
    /// F0.5 over all items (precision-weighted, R6). The headline scalar.
    pub f_half: f64,
    pub contacts_expected: usize,
    pub contacts_matched: usize,
    pub contact_accuracy: f64,
    pub distractor_count: usize,
    pub distractor_hits: usize,
    /// hits / distractor_count — the R6 over-extraction signal (lower better).
    pub distractor_fp_rate: f64,
    pub summary_ok: bool,
}

fn ratio(num: usize, den: usize) -> f64 {
    if den == 0 { 0.0 } else { num as f64 / den as f64 }
}

fn harmonic(p: f64, r: f64, beta_sq: f64) -> f64 {
    let den = beta_sq * p + r;
    if den == 0.0 { 0.0 } else { (1.0 + beta_sq) * p * r / den }
}

/// Grades one scenario. See module docs for the matching contract.
pub fn grade(truth: &GroundTruth, obs: &Observed) -> ScenarioScore {
    // Pre-tokenize once.
    let exp_tok: Vec<_> = truth.items.iter().map(|i| (i.kind.as_str(), token_set(&i.text))).collect();
    let obs_tok: Vec<_> = obs.items.iter().map(|i| (i.kind.as_str(), token_set(&i.text))).collect();

    // --- Same-kind greedy bipartite matching ---
    let mut exp_used = vec![false; exp_tok.len()];
    let mut obs_used = vec![false; obs_tok.len()];
    let mut pairs: Vec<(usize, usize, f64)> = Vec::new();
    for (ei, (ek, et)) in exp_tok.iter().enumerate() {
        for (oi, (ok, ot)) in obs_tok.iter().enumerate() {
            if ek == ok {
                let d = dice(et, ot);
                if d >= MATCH_THRESHOLD {
                    pairs.push((ei, oi, d));
                }
            }
        }
    }
    // Deterministic: highest Dice first, tie-break by indices.
    pairs.sort_by(|a, b| {
        b.2.partial_cmp(&a.2).unwrap().then(a.0.cmp(&b.0)).then(a.1.cmp(&b.1))
    });
    for (ei, oi, _) in &pairs {
        if !exp_used[*ei] && !obs_used[*oi] {
            exp_used[*ei] = true;
            obs_used[*oi] = true;
        }
    }

    // --- Confusion: unmatched expected vs unmatched cross-kind candidate ---
    let mut confusion: Vec<ConfusionEntry> = Vec::new();
    for (ei, (ek, et)) in exp_tok.iter().enumerate() {
        if exp_used[ei] { continue; }
        for (oi, (ok, ot)) in obs_tok.iter().enumerate() {
            if obs_used[oi] || ek == ok { continue; }
            if dice(et, ot) >= MATCH_THRESHOLD {
                bump_confusion(&mut confusion, ek, ok);
                // Diagnostic only: leave counts as FP/FN. Mark neither used —
                // a wrong-kind produced item is still a false positive.
                break;
            }
        }
    }

    let tp = exp_used.iter().filter(|u| **u).count();
    let fn_ = exp_tok.len() - tp;
    let fp = obs_tok.len() - obs_used.iter().filter(|u| **u).count();
    let overall = PrecisionRecall::from_counts(tp, fp, fn_);
    let f_half = harmonic(overall.precision, overall.recall, BETA_SQ);

    // --- Per-kind breakdown ---
    let mut kinds: Vec<String> = exp_tok.iter().map(|(k, _)| k.to_string())
        .chain(obs_tok.iter().map(|(k, _)| k.to_string())).collect();
    kinds.sort();
    kinds.dedup();
    let per_kind = kinds.iter().map(|kind| {
        let ktp = (0..exp_tok.len()).filter(|&i| exp_used[i] && exp_tok[i].0 == kind).count();
        let kfn = (0..exp_tok.len()).filter(|&i| !exp_used[i] && exp_tok[i].0 == kind).count();
        let kfp = (0..obs_tok.len()).filter(|&i| !obs_used[i] && obs_tok[i].0 == kind).count();
        let pr = PrecisionRecall::from_counts(ktp, kfp, kfn);
        KindScore {
            kind: kind.clone(),
            true_positives: pr.true_positives, false_positives: pr.false_positives,
            false_negatives: pr.false_negatives, precision: pr.precision, recall: pr.recall, f1: pr.f1,
        }
    }).collect();

    // --- R6 distractor false positives (any kind) ---
    let distractor_toks: Vec<_> = truth.distractors.iter().map(|d| token_set(d)).collect();
    let distractor_hits = obs_tok.iter().filter(|(_, ot)| {
        distractor_toks.iter().any(|dt| dice(dt, ot) >= MATCH_THRESHOLD)
    }).count();
    let distractor_fp_rate = ratio(distractor_hits, truth.distractors.len());

    // --- Contacts ---
    let (contacts_matched, contact_accuracy) = grade_contacts(truth, obs);

    ScenarioScore {
        overall, per_kind, confusion, f_half,
        contacts_expected: truth.contacts.len(),
        contacts_matched,
        contact_accuracy,
        distractor_count: truth.distractors.len(),
        distractor_hits,
        distractor_fp_rate,
        summary_ok: obs.summary_present == truth.expects_summary,
    }
}

fn bump_confusion(confusion: &mut Vec<ConfusionEntry>, expected: &str, produced: &str) {
    if let Some(e) = confusion.iter_mut().find(|c| c.expected_kind == expected && c.produced_kind == produced) {
        e.count += 1;
    } else {
        confusion.push(ConfusionEntry { expected_kind: expected.into(), produced_kind: produced.into(), count: 1 });
    }
}

fn grade_contacts(truth: &GroundTruth, obs: &Observed) -> (usize, f64) {
    let mut used = vec![false; obs.contacts.len()];
    let mut matched = 0;
    for ec in &truth.contacts {
        let en = token_set(&ec.name);
        for (oi, oc) in obs.contacts.iter().enumerate() {
            if used[oi] { continue; }
            if dice(&en, &token_set(&oc.name)) < MATCH_THRESHOLD { continue; }
            // trade only checked when expected trade is Some
            let trade_ok = match (&ec.trade, &oc.trade) {
                (Some(et), Some(ot)) => dice(&token_set(et), &token_set(ot)) >= MATCH_THRESHOLD,
                (Some(_), None) => false,
                (None, _) => true,
            };
            if trade_ok {
                used[oi] = true;
                matched += 1;
                break;
            }
        }
    }
    let accuracy = if truth.contacts.is_empty() { 1.0 } else { matched as f64 / truth.contacts.len() as f64 };
    (matched, accuracy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::{ExpectedContact, ExpectedItem, GroundTruth};

    fn item(kind: &str, text: &str) -> ExpectedItem {
        ExpectedItem { kind: kind.into(), text: text.into() }
    }
    fn obs_item(kind: &str, text: &str) -> ObservedItem {
        ObservedItem { kind: kind.into(), text: text.into() }
    }

    fn truth(items: Vec<ExpectedItem>) -> GroundTruth {
        GroundTruth { items, contacts: vec![], distractors: vec![], expects_summary: true }
    }

    #[test]
    fn perfect_extraction_scores_one() {
        let gt = truth(vec![item("todo", "order lumber"), item("safety", "loose railing")]);
        let obs = Observed {
            items: vec![obs_item("todo", "order the lumber"), obs_item("safety", "railing is loose")],
            contacts: vec![],
            summary_present: true,
        };
        let s = grade(&gt, &obs);
        assert_eq!(s.overall.true_positives, 2);
        assert_eq!(s.overall.false_positives, 0);
        assert_eq!(s.overall.false_negatives, 0);
        assert!((s.overall.precision - 1.0).abs() < 1e-9);
        assert!((s.overall.recall - 1.0).abs() < 1e-9);
        assert!((s.f_half - 1.0).abs() < 1e-9);
    }

    #[test]
    fn over_extraction_is_penalized_harder_than_under_by_f_half() {
        let gt = truth(vec![item("todo", "order lumber"), item("todo", "call inspector")]);
        // over-extraction: got both + 2 invented -> P=0.5, R=1.0
        let over = grade(&gt, &Observed {
            items: vec![
                obs_item("todo", "order lumber"), obs_item("todo", "call inspector"),
                obs_item("todo", "paint the fence"), obs_item("todo", "buy coffee"),
            ],
            contacts: vec![], summary_present: true,
        });
        // under-extraction: got one, missed one -> P=1.0, R=0.5
        let under = grade(&gt, &Observed {
            items: vec![obs_item("todo", "order lumber")],
            contacts: vec![], summary_present: true,
        });
        // R6: F0.5 weights precision, so the under-extractor scores HIGHER
        assert!(under.f_half > over.f_half, "under {} !> over {}", under.f_half, over.f_half);
    }

    #[test]
    fn wrong_kind_is_not_a_true_positive_but_shows_in_confusion() {
        let gt = truth(vec![item("safety", "loose railing")]);
        // right text, wrong kind (todo) -> FP + FN, and a confusion entry
        let s = grade(&gt, &Observed {
            items: vec![obs_item("todo", "loose railing")],
            contacts: vec![], summary_present: true,
        });
        assert_eq!(s.overall.true_positives, 0);
        assert_eq!(s.overall.false_positives, 1);
        assert_eq!(s.overall.false_negatives, 1);
        assert!(s.confusion.iter().any(|c| c.expected_kind == "safety" && c.produced_kind == "todo"));
    }

    #[test]
    fn distractor_hit_raises_r6_false_positive_rate() {
        let mut gt = truth(vec![item("todo", "order lumber")]);
        gt.distractors = vec!["might rain later maybe".into(), "grab lunch sometime".into()];
        // model wrongly turned a distractor into an item
        let s = grade(&gt, &Observed {
            items: vec![obs_item("todo", "order lumber"), obs_item("todo", "grab lunch sometime")],
            contacts: vec![], summary_present: true,
        });
        assert_eq!(s.distractor_count, 2);
        assert_eq!(s.distractor_hits, 1);
        assert!((s.distractor_fp_rate - 0.5).abs() < 1e-9);
    }

    #[test]
    fn contact_accuracy_matches_name_and_optional_trade() {
        let gt = GroundTruth {
            items: vec![], distractors: vec![], expects_summary: true,
            contacts: vec![
                ExpectedContact { name: "Dev".into(), trade: Some("framer".into()) },
                ExpectedContact { name: "Hank".into(), trade: None },
            ],
        };
        let s = grade(&gt, &Observed {
            items: vec![],
            contacts: vec![
                ObservedContact { name: "Dev".into(), trade: Some("framer".into()) },
                // Hank present, trade unknown — acceptable since expected trade is None
                ObservedContact { name: "Hank".into(), trade: None },
            ],
            summary_present: true,
        });
        assert_eq!(s.contacts_expected, 2);
        assert_eq!(s.contacts_matched, 2);
        assert!((s.contact_accuracy - 1.0).abs() < 1e-9);
    }

    #[test]
    fn wrong_trade_fails_the_contact_match() {
        let gt = GroundTruth {
            items: vec![], distractors: vec![], expects_summary: true,
            contacts: vec![ExpectedContact { name: "Dev".into(), trade: Some("framer".into()) }],
        };
        let s = grade(&gt, &Observed {
            items: vec![],
            contacts: vec![ObservedContact { name: "Dev".into(), trade: Some("plumber".into()) }],
            summary_present: true,
        });
        assert_eq!(s.contacts_matched, 0);
    }

    #[test]
    fn summary_presence_is_scored_against_expectation() {
        let gt = truth(vec![]);
        let missing = grade(&gt, &Observed { items: vec![], contacts: vec![], summary_present: false });
        assert!(!missing.summary_ok, "expected a summary, none produced");
        let present = grade(&gt, &Observed { items: vec![], contacts: vec![], summary_present: true });
        assert!(present.summary_ok);
    }

    #[test]
    fn per_kind_breakdown_is_reported() {
        let gt = truth(vec![item("todo", "order lumber"), item("safety", "loose railing")]);
        let s = grade(&gt, &Observed {
            items: vec![obs_item("todo", "order lumber")], // missed the safety item
            contacts: vec![], summary_present: true,
        });
        let todo = s.per_kind.iter().find(|k| k.kind == "todo").unwrap();
        assert!((todo.recall - 1.0).abs() < 1e-9);
        let safety = s.per_kind.iter().find(|k| k.kind == "safety").unwrap();
        assert!((safety.recall - 0.0).abs() < 1e-9);
    }
}
