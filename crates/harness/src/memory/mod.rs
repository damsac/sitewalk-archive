pub mod store;
pub mod tool;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Default word cap for a whole memory (spec §7: reflection compresses, never accumulates).
pub const DEFAULT_WORD_CAP: usize = 500;

/// Where a fact came from — drives eviction priority and debuggability.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FactSource {
    /// The agent's own deduction. First to be evicted.
    Inferred,
    /// The user said it outright.
    Stated,
    /// The user corrected the agent. Never auto-pruned; last to be evicted.
    Corrected,
}

impl FactSource {
    pub fn rank(self) -> u8 {
        match self {
            FactSource::Inferred => 0,
            FactSource::Stated => 1,
            FactSource::Corrected => 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub text: String,
    /// Unix seconds when this entry was last added, confirmed, or re-mentioned.
    pub last_touched: u64,
    pub source: FactSource,
    /// Session id this fact came from, if known.
    pub session: Option<String>,
}

/// Sectioned agent memory. Section names are consumer-defined strings
/// (e.g. "vocabulary", "people", "projects", "preferences").
/// The "vocabulary" section is read by the STT biasing layer in Plan 05
/// via [`Memory::section_texts`].
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Memory {
    pub sections: BTreeMap<String, Vec<MemoryEntry>>,
}

impl Memory {
    /// Adds `text` to `section`, or refreshes `last_touched` if the exact text exists.
    /// Defaults provenance to [`FactSource::Inferred`] with no session.
    pub fn remember(&mut self, section: &str, text: &str, now: u64) {
        self.remember_from(section, text, now, FactSource::Inferred, None);
    }

    /// Full-provenance remember. On an existing exact text: refreshes `last_touched`;
    /// upgrades `source`/`session` only if the new source ranks higher (never downgrades).
    pub fn remember_from(
        &mut self,
        section: &str,
        text: &str,
        now: u64,
        source: FactSource,
        session: Option<String>,
    ) {
        let entries = self.sections.entry(section.to_string()).or_default();
        match entries.iter_mut().find(|e| e.text == text) {
            Some(e) => {
                e.last_touched = now;
                if source.rank() > e.source.rank() {
                    e.source = source;
                    e.session = session;
                }
            }
            None => entries.push(MemoryEntry {
                text: text.to_string(),
                last_touched: now,
                source,
                session,
            }),
        }
    }

    /// Removes the exact `text` from `section`. Returns whether anything was removed.
    /// Sections left empty are dropped.
    pub fn forget(&mut self, section: &str, text: &str) -> bool {
        let Some(entries) = self.sections.get_mut(section) else {
            return false;
        };
        let before = entries.len();
        entries.retain(|e| e.text != text);
        let removed = entries.len() < before;
        if entries.is_empty() {
            self.sections.remove(section);
        }
        removed
    }

    /// Total whitespace-separated words across all entry texts.
    pub fn word_count(&self) -> usize {
        self.sections
            .values()
            .flatten()
            .map(|e| e.text.split_whitespace().count())
            .sum()
    }

    /// Entry texts of one section, in insertion order. Empty if the section is absent.
    pub fn section_texts(&self, section: &str) -> Vec<&str> {
        self.sections
            .get(section)
            .map(|es| es.iter().map(|e| e.text.as_str()).collect())
            .unwrap_or_default()
    }

    /// Markdown rendering for prompt injection: `## section` headers, `- ` entries,
    /// sections in BTreeMap (alphabetical) order. Empty memory renders as "".
    pub fn to_prompt(&self) -> String {
        let mut out = String::new();
        for (name, entries) in &self.sections {
            if entries.is_empty() {
                continue;
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("## ");
            out.push_str(name);
            out.push('\n');
            for e in entries {
                out.push_str("- ");
                out.push_str(&e.text);
                out.push('\n');
            }
        }
        out
    }

    /// Removes entries whose `last_touched` is older than `max_age_secs` before `now`
    /// (spec Rev 3: forgetting is a feature). User corrections are never auto-pruned.
    /// Returns how many entries were removed.
    pub fn prune_stale(&mut self, now: u64, max_age_secs: u64) -> usize {
        let cutoff = now.saturating_sub(max_age_secs);
        let mut removed = 0;
        self.sections.retain(|_, entries| {
            let before = entries.len();
            entries.retain(|e| e.source == FactSource::Corrected || e.last_touched >= cutoff);
            removed += before - entries.len();
            !entries.is_empty()
        });
        removed
    }

    /// Drops entries until `word_count() <= cap`, evicting by ascending
    /// `(source rank, last_touched, section name)` — inferred-oldest first,
    /// corrected last. Ties within the same section and timestamp resolve by
    /// insertion order (deterministic, since `min_by` keeps the first minimum
    /// in iteration order). Returns how many entries were removed.
    pub fn clamp_to_cap(&mut self, cap: usize) -> usize {
        let mut removed = 0;
        while self.word_count() > cap {
            let next = self
                .sections
                .iter()
                .flat_map(|(name, entries)| {
                    entries
                        .iter()
                        .map(move |e| ((e.source.rank(), e.last_touched, name.clone()), e.text.clone()))
                })
                .min_by(|a, b| a.0.cmp(&b.0));
            let Some(((_, _, section), text)) = next else { break };
            self.forget(&section, &text);
            removed += 1;
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_with(section: &str, texts: &[(&str, u64)]) -> Memory {
        let mut m = Memory::default();
        for (t, at) in texts {
            m.remember(section, t, *at);
        }
        m
    }

    #[test]
    fn remember_adds_and_touches_existing() {
        let mut m = Memory::default();
        m.remember("people", "Dev \u{2014} framer", 100);
        m.remember("people", "Dev \u{2014} framer", 200); // same text: touch, don't duplicate
        assert_eq!(m.sections["people"].len(), 1);
        assert_eq!(m.sections["people"][0].last_touched, 200);
        assert_eq!(m.sections["people"][0].source, FactSource::Inferred);
    }

    #[test]
    fn source_upgrades_but_never_downgrades() {
        let mut m = Memory::default();
        m.remember_from("people", "Dev \u{2014} framer", 100, FactSource::Corrected, Some("s3".into()));
        m.remember("people", "Dev \u{2014} framer", 200); // inferred touch
        let e = &m.sections["people"][0];
        assert_eq!(e.last_touched, 200);
        assert_eq!(e.source, FactSource::Corrected);
        assert_eq!(e.session.as_deref(), Some("s3"));
        m.remember_from("people", "Dev \u{2014} framer", 300, FactSource::Stated, Some("s9".into()));
        assert_eq!(m.sections["people"][0].source, FactSource::Corrected, "no downgrade");
    }

    #[test]
    fn forget_removes_and_reports() {
        let mut m = mem_with("people", &[("Dev \u{2014} framer", 100)]);
        assert!(m.forget("people", "Dev \u{2014} framer"));
        assert!(!m.forget("people", "Dev \u{2014} framer"));
        assert!(!m.sections.contains_key("people"), "empty sections are dropped");
    }

    #[test]
    fn word_count_counts_entry_words() {
        let m = mem_with("vocabulary", &[("bark mulch", 1), ("french drain", 1)]);
        assert_eq!(m.word_count(), 4);
    }

    #[test]
    fn to_prompt_renders_sections_in_order() {
        let mut m = mem_with("people", &[("Dev \u{2014} framer", 1)]);
        m.remember("jobs", "Johnson remodel \u{2014} active", 1);
        assert_eq!(
            m.to_prompt(),
            "## jobs\n- Johnson remodel \u{2014} active\n\n## people\n- Dev \u{2014} framer\n"
        );
    }

    #[test]
    fn to_prompt_empty_memory_is_empty_string() {
        assert_eq!(Memory::default().to_prompt(), "");
    }

    #[test]
    fn section_texts_accessor() {
        let m = mem_with("vocabulary", &[("skid steer", 1), ("french drain", 1)]);
        assert_eq!(m.section_texts("vocabulary"), vec!["skid steer", "french drain"]);
        assert!(m.section_texts("nope").is_empty());
    }

    #[test]
    fn prune_stale_drops_old_entries_and_empty_sections() {
        let mut m = Memory::default();
        m.remember("people", "old contact", 100);
        m.remember("people", "fresh contact", 900);
        m.remember("jobs", "ancient job", 50);
        let removed = m.prune_stale(1000, 500); // older than 500s ago goes
        assert_eq!(removed, 2);
        assert_eq!(m.section_texts("people"), vec!["fresh contact"]);
        assert!(!m.sections.contains_key("jobs"));
    }

    #[test]
    fn clamp_to_cap_drops_oldest_first() {
        let mut m = Memory::default();
        m.remember("a", "one two three", 100); // 3 words, oldest
        m.remember("b", "four five", 200); // 2 words
        m.remember("c", "six seven eight nine", 300); // 4 words, newest
        let removed = m.clamp_to_cap(6);
        assert_eq!(removed, 1, "dropping the oldest entry reaches the cap");
        assert_eq!(m.word_count(), 6);
        assert!(!m.sections.contains_key("a"));
        assert_eq!(m.section_texts("b"), vec!["four five"]);
        assert_eq!(m.section_texts("c"), vec!["six seven eight nine"]);
    }

    #[test]
    fn clamp_to_cap_noop_when_within() {
        let mut m = Memory::default();
        m.remember("a", "one two", 100);
        assert_eq!(m.clamp_to_cap(10), 0);
        assert_eq!(m.word_count(), 2);
    }

    #[test]
    fn corrected_facts_survive_pruning_and_evict_last() {
        let mut m = Memory::default();
        m.remember_from("people", "Dev not Dave", 10, FactSource::Corrected, None);
        m.remember("people", "likes early starts", 9);
        assert_eq!(m.prune_stale(1000, 100), 1, "only the inferred fact prunes");
        assert_eq!(m.section_texts("people"), vec!["Dev not Dave"]);

        m.remember_from("a", "one two three four", 500, FactSource::Stated, None);
        // cap forces eviction: inferred gone already; stated (rank 1) goes before corrected (rank 2)
        m.clamp_to_cap(3);
        assert_eq!(m.section_texts("people"), vec!["Dev not Dave"]);
        assert!(!m.sections.contains_key("a"));

        // Corrected facts are last to go, not immune: with only Corrected
        // entries left, clamp_to_cap still evicts to honor the cap.
        m.remember_from("people", "prefers text over calls", 20, FactSource::Corrected, None);
        assert_eq!(m.word_count(), 7); // "Dev not Dave" (3) + new entry (4)
        let removed = m.clamp_to_cap(4);
        assert_eq!(removed, 1, "oldest corrected entry is evicted");
        assert!(m.word_count() <= 4);
        assert_eq!(m.section_texts("people"), vec!["prefers text over calls"]);
    }
}
