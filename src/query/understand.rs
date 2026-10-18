//! Understanding: a search request becomes what the other stages work with: its words and the parts in quotes, whether
//! it searches code, which kinds it shows, and the prompt it's embedded with. Pure.

use embedder::prompts;
use protocol::{Kind, Search};

/// "code: retry with backoff": a search of code, and only of code. Without the prefix (or the code filter), a search
/// never shows code: a project's thousands of stretches would bury everything else.
pub const CODE_PREFIX: &str = "code:";

/// A search request, understood.
pub struct Query {
    /// The words searched for, as the full-text index splits them.
    pub words: Vec<String>,
    /// The parts in quotes ("INV-2024-0113", or curly quotes): words that must be there, in that order.
    pub phrases: Vec<Vec<String>>,
    /// The last word is matched as a prefix, while someone types it.
    pub prefix_last: bool,
    /// The words decide where they're found: there are words the model can't read (a number or a code such as
    /// INV-2024-0113), it's a search of code, or words are in quotes. Other words the model reads, so where they're
    /// found only breaks near-ties.
    pub words_decide: bool,
    /// A search of code: the prefix, or the code filter alone.
    code: bool,
    kinds: Vec<Kind>,
    /// What's searched for, without the prefix.
    text: String,
}

impl Query {
    pub fn of(request: &Search) -> Self {
        let prefix = has_prefix(&request.q);
        let code = prefix || request.kinds == [Kind::Code];
        let text = if prefix { request.q.trim_start()[CODE_PREFIX.len()..].trim() } else { request.q.trim() };
        let quoted = text.replace(['“', '”', '„'], "\"");
        let phrases: Vec<Vec<String>> =
            quoted.split('"').skip(1).step_by(2).map(words_of).filter(|words| !words.is_empty()).collect();
        let words = words_of(text);
        let words_decide = code || !phrases.is_empty() || words.iter().any(|word| word.chars().any(char::is_numeric));
        Self {
            words,
            phrases,
            prefix_last: request.prefix_last,
            words_decide,
            code,
            kinds: request.kinds.clone(),
            text: text.to_string(),
        }
    }

    /// Whether it shows files of this kind: only code in a search of code, else the kinds asked for, never code.
    pub fn wants(&self, kind: Kind) -> bool {
        if self.code {
            kind == Kind::Code
        } else {
            kind != Kind::Code && (self.kinds.is_empty() || self.kinds.contains(&kind))
        }
    }

    /// The prompt it's embedded with (quotes don't mean anything to the model).
    pub fn prompt(&self) -> String {
        let text = self.text.replace(['"', '“', '”', '„'], "");
        if self.code { prompts::code_query(&text) } else { prompts::query(&text) }
    }
}

fn has_prefix(q: &str) -> bool {
    q.trim_start().get(..CODE_PREFIX.len()).is_some_and(|start| start.eq_ignore_ascii_case(CODE_PREFIX))
}

/// The words of a query, as the full-text index splits them (letters and digits; everything else separates).
pub fn words_of(query: &str) -> Vec<String> {
    query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(|word| word.to_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(q: &str) -> Query {
        Query::of(&Search { q: q.into(), ..Default::default() })
    }

    #[test]
    fn quoted_parts_are_phrases() {
        let quoted = query(r#"invoice "INV-2024-0113" from “Altink Design”"#);
        assert_eq!(quoted.phrases, vec![vec!["inv", "2024", "0113"], vec!["altink", "design"]]);
        assert!(!quoted.prompt().contains('"'));
        assert!(query("no quotes here").phrases.is_empty());
    }

    #[test]
    fn numbers_codes_and_quotes_decide() {
        let decides = |q: &str| query(q).words_decide;
        assert!(decides("INV-2024-0113"));
        assert!(decides("tax return 2023"));
        assert!(decides(r#"the "Altink Design" invoice"#));
        assert!(decides("code: refreshAccessToken"));
        assert!(!decides("a steam train"));
        assert!(!decides("Bram"));
    }

    #[test]
    fn code_is_shown_only_in_a_search_of_code() {
        assert!(query("code: retry").wants(Kind::Code));
        assert!(!query("code: retry").wants(Kind::Document));
        assert!(!query("retry").wants(Kind::Code));
        let documents = Query::of(&Search { q: "retry".into(), kinds: vec![Kind::Document], ..Default::default() });
        assert!(documents.wants(Kind::Document) && !documents.wants(Kind::Image));
        assert_eq!(query("Code:  retry with backoff").words, ["retry", "with", "backoff"]);
    }
}
