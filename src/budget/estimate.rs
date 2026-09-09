//! Character based token estimate. DeepSeek ships no local tokenizer, so the
//! same conservative function serves the chunk limit in `triage`, the context
//! check in the tool loop, and the pre-call budget check.

/// Guessing low is the expensive direction, so every estimate carries this.
const SAFETY_FACTOR: f64 = 1.2;
const ASCII_CHARS_PER_TOKEN: f64 = 4.0;

/// ASCII runs about 4 characters per token, CJK about 1, and the whole thing
/// gets a 1.2 safety factor because guessing low is the expensive direction.
pub fn estimate_tokens(text: &str) -> u32 {
    let mut ascii = 0u64;
    let mut wide = 0u64;
    for character in text.chars() {
        if character.is_ascii() {
            ascii += 1;
        } else {
            wide += 1;
        }
    }
    let raw = ascii as f64 / ASCII_CHARS_PER_TOKEN + wide as f64;
    (raw * SAFETY_FACTOR).ceil() as u32
}

/// The same rates applied to a byte allowance rather than to real text, for
/// reserving room for output nobody has produced yet. Tool output is
/// diagnostics, so it is counted at the ASCII rate.
pub fn estimate_ascii_tokens(bytes: u64) -> u32 {
    let raw = bytes as f64 / ASCII_CHARS_PER_TOKEN;
    (raw * SAFETY_FACTOR).ceil().min(u32::MAX as f64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_costs_nothing() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn ascii_counts_four_characters_to_a_token() {
        // 16 characters / 4 = 4 tokens, times the 1.2 factor.
        assert_eq!(estimate_tokens("abcdefghijklmnop"), 5);
    }

    #[test]
    fn cjk_counts_one_character_to_a_token() {
        assert_eq!(estimate_tokens("数组越界了"), 6);
    }

    #[test]
    fn a_byte_allowance_uses_the_same_ascii_rate() {
        // 64 KiB of tool output / 4 = 16384 tokens, times the 1.2 factor.
        assert_eq!(estimate_ascii_tokens(65_536), 19_661);
        assert_eq!(estimate_ascii_tokens(0), 0);
    }

    #[test]
    fn mixed_text_adds_the_two_rates() {
        // 8 ascii / 4 = 2, plus 2 wide = 4, times 1.2 = 4.8 -> 5.
        assert_eq!(estimate_tokens("abcdefgh越界"), 5);
    }
}
