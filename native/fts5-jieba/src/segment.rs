//! Segmentation engine: jieba for Chinese, fold + optional Porter stem for
//! English. Pure logic, independent of the FTS5 FFI so it can be unit-tested.
//!
//! Strategy (plan §4.1 simplified path): hand the whole string to jieba. jieba
//! segments CJK by dictionary and emits runs of ASCII/Latin as their own tokens
//! with correct byte offsets. We then post-process each token:
//!   - normalize (NFD, strip diacritics, lowercase) -> the indexed token
//!   - if it's an English word and stemming is on, ALSO emit its Porter stem as
//!     a colocated token, so `running` indexes both `running` and `run`.
//!
//! Byte offsets ALWAYS point into the original input (jieba's byte_start/
//! byte_end), never into the normalized text — FTS5 needs source offsets for
//! snippet()/highlight().

use jieba_rs::{Jieba, TokenizeMode};
use rust_stemmers::{Algorithm, Stemmer};

/// A unit of output: normalized text + byte span in the original input +
/// whether it sits at the same position as the previous token (synonyms/stems).
pub struct Emit {
    pub text: String,
    pub byte_start: usize,
    pub byte_end: usize,
    pub colocated: bool,
}

/// Why FTS5 asked us to tokenize. Chooses jieba granularity.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Indexing a document — cut fine (Search mode) for recall.
    Document,
    /// A MATCH query — cut coarse (Default mode) for precision.
    Query,
}

/// Owns the (expensive to build) jieba dictionary + stemmer, reused across
/// every xTokenize call.
pub struct Engine {
    jieba: Jieba,
    stemmer: Stemmer,
    stem_english: bool,
}

impl Engine {
    pub fn new(stem_english: bool) -> Self {
        Engine {
            jieba: Jieba::new(),
            stemmer: Stemmer::create(Algorithm::English),
            stem_english,
        }
    }

    /// Segment `input`, invoking `push` for each token to emit in order.
    /// `push` returns false to abort early (mirrors xToken returning non-OK).
    ///
    /// We first split the input into maximal CJK vs non-CJK runs. CJK runs go
    /// to jieba; non-CJK (Latin/digit/punct) runs are word-split here. This is
    /// necessary because jieba fragments accented Latin (`Café` -> `Caf`+`é`),
    /// which would defeat diacritic folding. Byte offsets from each path are
    /// rebased onto the original input.
    pub fn segment(&self, input: &str, reason: Reason, mut push: impl FnMut(Emit) -> bool) {
        for run in script_runs(input) {
            let base = run.start;
            let ok = match run.kind {
                RunKind::Cjk => self.segment_cjk(run.text, base, reason, &mut push),
                RunKind::Other => self.segment_latin(run.text, base, &mut push),
            };
            if !ok {
                return;
            }
        }
    }

    /// Hand a pure-CJK run to jieba. Returns false if `push` aborted.
    fn segment_cjk(
        &self,
        text: &str,
        base: usize,
        reason: Reason,
        push: &mut impl FnMut(Emit) -> bool,
    ) -> bool {
        let mode = match reason {
            Reason::Document => TokenizeMode::Search,
            Reason::Query => TokenizeMode::Default,
        };
        // hmm=true: let jieba discover words not in the dictionary.
        for tok in self.jieba.tokenize(text, mode, true) {
            if tok.word.trim().is_empty() {
                continue;
            }
            let normalized = normalize(tok.word);
            if normalized.is_empty() {
                continue;
            }
            // CJK tokens are never English; no stemming.
            if !push(Emit {
                text: normalized,
                byte_start: base + tok.start,
                byte_end: base + tok.end,
                colocated: false,
            }) {
                return false;
            }
        }
        true
    }

    /// Word-split a non-CJK run: maximal runs of alphanumeric-or-combining
    /// chars are words; everything else (whitespace, punctuation) separates.
    /// Each word is normalized, then optionally emits a colocated Porter stem.
    fn segment_latin(&self, text: &str, base: usize, push: &mut impl FnMut(Emit) -> bool) -> bool {
        let mut it = text.char_indices().peekable();
        while let Some(&(start, _)) = it.peek() {
            // skip non-word chars
            if !is_word_char(text[start..].chars().next().unwrap()) {
                it.next();
                continue;
            }
            // consume a maximal word
            let mut end = start;
            while let Some(&(i, c)) = it.peek() {
                if is_word_char(c) {
                    end = i + c.len_utf8();
                    it.next();
                } else {
                    break;
                }
            }
            let word = &text[start..end];
            let normalized = normalize(word);
            if normalized.is_empty() {
                continue;
            }
            if !push(Emit {
                text: normalized.clone(),
                byte_start: base + start,
                byte_end: base + end,
                colocated: false,
            }) {
                return false;
            }
            // Colocated Porter stem for English words (M4). Base the decision on
            // the FOLDED form so accented words (café->cafe) can still stem.
            // Only emit when the stem differs, to avoid a redundant duplicate.
            if self.stem_english && is_stemmable(&normalized) {
                let stem = self.stemmer.stem(&normalized).to_string();
                if stem != normalized
                    && !stem.is_empty()
                    && !push(Emit {
                        text: stem,
                        byte_start: base + start,
                        byte_end: base + end,
                        colocated: true,
                    })
                {
                    return false;
                }
            }
        }
        true
    }
}

/// A word char within a non-CJK run: alphanumeric, or a combining diacritic so
/// NFD-decomposed accents don't split a word.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || is_combining_diacritic(c)
}

/// Whether a (already-normalized) token should be Porter-stemmed: pure ASCII
/// letters. Numbers and any residual non-ASCII are left as-is.
fn is_stemmable(normalized: &str) -> bool {
    !normalized.is_empty() && normalized.chars().all(|c| c.is_ascii_alphabetic())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RunKind {
    Cjk,
    Other,
}

struct Run<'a> {
    kind: RunKind,
    text: &'a str,
    start: usize,
}

/// Split `input` into maximal runs of CJK vs non-CJK characters, preserving
/// byte offsets.
fn script_runs(input: &str) -> Vec<Run<'_>> {
    let mut runs = Vec::new();
    let mut run_start = 0usize;
    let mut cur: Option<RunKind> = None;
    for (i, c) in input.char_indices() {
        let kind = if is_cjk(c) {
            RunKind::Cjk
        } else {
            RunKind::Other
        };
        match cur {
            Some(k) if k == kind => {}
            Some(k) => {
                runs.push(Run {
                    kind: k,
                    text: &input[run_start..i],
                    start: run_start,
                });
                run_start = i;
                cur = Some(kind);
            }
            None => {
                run_start = i;
                cur = Some(kind);
            }
        }
    }
    if let Some(k) = cur {
        runs.push(Run {
            kind: k,
            text: &input[run_start..],
            start: run_start,
        });
    }
    runs
}

/// CJK ideographs (Chinese). Ranges chosen for Chinese full-text search:
/// CJK Unified + Extension A + compatibility ideographs.
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3400..=0x4DBF   // CJK Ext A
        | 0x4E00..=0x9FFF // CJK Unified
        | 0xF900..=0xFAFF // CJK Compatibility Ideographs
        | 0x20000..=0x2A6DF // CJK Ext B
    )
}

/// Normalize a token for indexing: decompose, drop combining diacritics, and
/// lowercase. `café` -> `cafe`, `Straße` -> `strasse`? (No: ß has no NFD
/// decomposition; lowercasing keeps `ß`. Diacritic folding only.)
pub fn normalize(segment: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let mut out = String::with_capacity(segment.len());
    for c in segment.nfd() {
        if is_combining_diacritic(c) {
            continue;
        }
        if c.is_ascii() {
            out.push(c.to_ascii_lowercase());
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// Combining diacritical marks block U+0300..=U+036F.
fn is_combining_diacritic(c: char) -> bool {
    ('\u{0300}'..='\u{036f}').contains(&c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(engine: &Engine, input: &str, reason: Reason) -> Vec<(String, usize, usize, bool)> {
        let mut out = vec![];
        engine.segment(input, reason, |e| {
            out.push((e.text, e.byte_start, e.byte_end, e.colocated));
            true
        });
        out
    }

    #[test]
    fn normalizes_diacritics_and_case() {
        assert_eq!(normalize("DïācRîtįcs"), "diacritics");
        assert_eq!(normalize("Café"), "cafe");
    }

    #[test]
    fn chinese_is_segmented() {
        let e = Engine::new(false);
        let toks = collect(&e, "中华人民共和国", Reason::Query);
        let words: Vec<&str> = toks.iter().map(|t| t.0.as_str()).collect();
        // jieba should produce multi-char words, not just single chars.
        assert!(words.contains(&"中华人民共和国") || words.contains(&"中华"));
    }

    #[test]
    fn english_offsets_are_byte_offsets_into_original() {
        let e = Engine::new(false);
        // leading CJK pushes the English word's byte offset past its char index
        let toks = collect(&e, "中文abc", Reason::Query);
        let abc = toks.iter().find(|t| t.0 == "abc").expect("abc token");
        assert_eq!(abc.1, 6, "byte_start after two 3-byte CJK chars");
        assert_eq!(abc.2, 9);
    }

    #[test]
    fn stemming_emits_colocated_stem() {
        let e = Engine::new(true);
        let toks = collect(&e, "running", Reason::Document);
        // original 'running' (colocated=false) then stem 'run' (colocated=true)
        assert!(toks.iter().any(|t| t.0 == "running" && !t.3));
        assert!(toks.iter().any(|t| t.0 == "run" && t.3));
    }

    #[test]
    fn no_redundant_stem_when_equal() {
        let e = Engine::new(true);
        let toks = collect(&e, "cat", Reason::Document);
        // 'cat' stems to 'cat' -> only one emit, not a colocated duplicate.
        assert_eq!(toks.iter().filter(|t| t.0 == "cat").count(), 1);
    }

    #[test]
    fn accented_latin_stays_one_word() {
        // Regression: jieba fragments "Café" into "Caf"+"é", which broke
        // diacritic folding. Latin runs must be word-split here, not by jieba.
        let e = Engine::new(false);
        let toks = collect(&e, "Café résumé", Reason::Document);
        let words: Vec<&str> = toks.iter().map(|t| t.0.as_str()).collect();
        assert_eq!(words, vec!["cafe", "resume"]);
        // and byte offsets still cover the original accented spans
        assert_eq!(toks[0].1, 0);
        assert_eq!(toks[0].2, "Café".len()); // 5 bytes
    }

    #[test]
    fn cjk_english_boundary_split() {
        // No space between scripts: "苹果iPhone" must split into a CJK run and
        // a Latin run, each tokenized by its own path.
        let e = Engine::new(false);
        let toks = collect(&e, "苹果iPhone", Reason::Query);
        let words: Vec<&str> = toks.iter().map(|t| t.0.as_str()).collect();
        assert!(words.contains(&"苹果"));
        assert!(words.contains(&"iphone"));
    }
}
