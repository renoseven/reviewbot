//! Cut oversized text at the tail and say how much was cut, so the model can
//! tell a short answer from a clipped one.

#[derive(Clone, Debug, PartialEq)]
pub struct Truncated {
    pub text: String,
    pub omitted_bytes: usize,
}

impl Truncated {
    pub fn was_truncated(&self) -> bool {
        self.omitted_bytes > 0
    }
}

/// Keep the head, drop the tail. The cut lands on a character boundary.
pub fn truncate(text: &str, max_bytes: usize) -> Truncated {
    if text.len() <= max_bytes {
        return Truncated {
            text: text.to_string(),
            omitted_bytes: 0,
        };
    }
    let mut cut = max_bytes;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    Truncated {
        text: text[..cut].to_string(),
        omitted_bytes: text.len() - cut,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_returned_untouched() {
        let result = truncate("hello", 16);
        assert_eq!(result.text, "hello");
        assert!(!result.was_truncated());
    }

    #[test]
    fn the_cut_lands_on_a_character_boundary() {
        let result = truncate("数组越界", 5);
        assert_eq!(result.text, "数");
        assert_eq!(result.omitted_bytes, 9);
    }
}
