//! Token-budgeted context assembly (spec §4: token budget is a first-class
//! constraint). Uses a documented ~4-chars-per-token approximation — good
//! enough for budget enforcement; exact counts come from provider usage.

/// Approximate token count: ceil(chars / 4).
pub fn approx_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// One named, budgeted block of prompt context.
pub struct ContextSection {
    pub title: String,
    pub content: String,
    pub budget_tokens: usize,
}

pub struct AssembledContext {
    pub text: String,
    pub approx_tokens: usize,
    /// Titles of sections that had to be cut to fit their budget.
    pub truncated_sections: Vec<String>,
}

pub struct ContextAssembler;

impl ContextAssembler {
    /// Renders sections as `## title\ncontent`, truncating each to its own
    /// budget (by chars = tokens * 4) with a `…[truncated]` marker.
    /// Empty sections are skipped entirely.
    pub fn assemble(sections: &[ContextSection]) -> AssembledContext {
        let mut parts = Vec::new();
        let mut truncated_sections = Vec::new();

        for section in sections {
            if section.content.is_empty() {
                continue;
            }
            let budget_chars = section.budget_tokens * 4;
            let content = if section.content.chars().count() > budget_chars {
                truncated_sections.push(section.title.clone());
                let cut: String = section.content.chars().take(budget_chars).collect();
                format!("{cut}\n…[truncated]")
            } else {
                section.content.clone()
            };
            parts.push(format!("## {}\n{}", section.title, content));
        }

        let text = parts.join("\n\n");
        AssembledContext { approx_tokens: approx_tokens(&text), text, truncated_sections }
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn approx_tokens_is_chars_over_four_rounded_up() {
        assert_eq!(approx_tokens(""), 0);
        assert_eq!(approx_tokens("abcd"), 1);
        assert_eq!(approx_tokens("abcde"), 2);
    }

    #[test]
    fn within_budget_sections_pass_through() {
        let out = ContextAssembler::assemble(&[
            ContextSection { title: "memory".into(), content: "knows stuff".into(), budget_tokens: 100 },
            ContextSection { title: "recent".into(), content: "items".into(), budget_tokens: 100 },
        ]);
        assert_eq!(out.text, "## memory\nknows stuff\n\n## recent\nitems");
        assert!(out.truncated_sections.is_empty());
        assert_eq!(out.approx_tokens, approx_tokens(&out.text));
    }

    #[test]
    fn over_budget_section_is_truncated_with_marker() {
        let long = "word ".repeat(100); // 500 chars
        let out = ContextAssembler::assemble(&[ContextSection {
            title: "transcript".into(),
            content: long,
            budget_tokens: 10, // 40 chars
        }]);
        assert_eq!(out.truncated_sections, vec!["transcript".to_string()]);
        assert!(out.text.contains("…[truncated]"));
        // content portion respects the budget: 40 chars + marker
        let body = out.text.strip_prefix("## transcript\n").unwrap();
        let content_part = body.strip_suffix("\n…[truncated]").unwrap();
        assert_eq!(content_part.chars().count(), 40);
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        let content = "é".repeat(50);
        let out = ContextAssembler::assemble(&[ContextSection {
            title: "t".into(),
            content,
            budget_tokens: 5, // 20 chars
        }]);
        assert!(out.text.contains(&"é".repeat(20)));
        assert_eq!(out.truncated_sections.len(), 1);
    }

    #[test]
    fn empty_sections_are_skipped() {
        let out = ContextAssembler::assemble(&[
            ContextSection { title: "empty".into(), content: String::new(), budget_tokens: 10 },
            ContextSection { title: "full".into(), content: "hi".into(), budget_tokens: 10 },
        ]);
        assert_eq!(out.text, "## full\nhi");
    }
}
