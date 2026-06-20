//! Real BPE token counting for precise context accounting.
//!
//! The old approach multiplied a character count by a fixed chars-per-token guess. That is wildly
//! inaccurate for a coding agent's transcript (code, JSON, tool output tokenize very differently
//! from prose), which made the context gauge lie and let the trimmed input overflow the model's
//! window. Here we count tokens with a real BPE tokenizer (`o200k_base`, the GPT-4o/5 vocabulary
//! bundled in-crate, so it works offline). It is exact for OpenAI/Codex models and a close
//! approximation for others — Claude/Gemini/Llama tokenizers differ only modestly in magnitude —
//! which is far better than a chars/token heuristic for deciding when to trim and compact.
//!
//! Counts are cached by content hash: transcript messages are immutable once recorded, so the
//! (relatively expensive) BPE pass runs once per distinct string and is essentially free thereafter
//! — important because the gauge recomputes the whole transcript frequently.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};

use tiktoken_rs::{o200k_base, CoreBPE};

fn bpe() -> &'static CoreBPE {
    static BPE: OnceLock<CoreBPE> = OnceLock::new();
    BPE.get_or_init(|| o200k_base().expect("bundled o200k_base vocabulary always loads"))
}

fn cache() -> &'static Mutex<HashMap<u64, usize>> {
    static CACHE: OnceLock<Mutex<HashMap<u64, usize>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Per-message framing overhead (role marker + delimiters) the model also pays, mirrored so the
/// transcript estimate tracks the real request size. ~4 tokens is the standard chat-format budget.
pub const PER_MESSAGE_OVERHEAD: usize = 4;

/// Exact BPE token count of `text` (no framing). Cached by content hash so repeated counts of the
/// same (immutable) message are O(1).
pub fn count_text(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    let key = h.finish();
    if let Some(&n) = cache().lock().unwrap().get(&key) {
        return n;
    }
    let n = bpe().encode_with_special_tokens(text).len();
    cache().lock().unwrap().insert(key, n);
    n
}

/// Token count of one message including the chat-format framing overhead.
pub fn count_message(content: &str) -> usize {
    count_text(content) + PER_MESSAGE_OVERHEAD
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_nonzero_and_scale_with_length() {
        assert_eq!(count_text(""), 0);
        let short = count_text("hello world");
        let long = count_text(&"hello world ".repeat(100));
        assert!(short > 0, "non-empty text has tokens");
        assert!(
            long > short * 50,
            "100x the text is ~100x the tokens: {short} vs {long}"
        );
    }

    #[test]
    fn caching_returns_the_same_count() {
        let s = "the quick brown fox jumps over the lazy dog";
        assert_eq!(count_text(s), count_text(s));
    }

    #[test]
    fn message_adds_framing_overhead() {
        assert_eq!(count_message("x"), count_text("x") + PER_MESSAGE_OVERHEAD);
    }
}
