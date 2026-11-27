//! Character-safe text trimming, shared by every crate that cuts text
//! for a label, a card or an error line. Counts are in characters, so a
//! cut never lands inside a multi-byte one.

/// The first `limit` characters of `text`, borrowed.
pub fn prefix_chars(text: &str, limit: usize) -> &str {
    match text.char_indices().nth(limit) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}

/// `text` in at most `limit` characters: unchanged when it fits, else
/// cut with a trailing `…` that counts toward the limit.
pub fn ellipsize(text: &str, limit: usize) -> String {
    if text.chars().nth(limit).is_none() {
        return text.to_string();
    }
    let mut out = prefix_chars(text, limit.saturating_sub(1)).to_string();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuts_land_on_characters_and_count_the_ellipsis() {
        assert_eq!(prefix_chars("héllo", 2), "hé");
        assert_eq!(prefix_chars("hi", 5), "hi");
        assert_eq!(ellipsize("hello", 5), "hello");
        assert_eq!(ellipsize("hello!", 5), "hell…");
        assert_eq!(ellipsize("ééééé!", 3).chars().count(), 3);
        assert_eq!(ellipsize("abc", 0), "…");
    }
}
