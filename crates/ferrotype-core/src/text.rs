//! Getting text ready to type, and short previews of it for the history list.

/// Spaces nobody types on a keyboard (copied web text is full of them): no-break space, the
/// fixed-width spaces U+2000 to U+200A, narrow no-break space and medium mathematical space.
const ODD_SPACES: [u32; 14] =
    [0xA0, 0x2000, 0x2001, 0x2002, 0x2003, 0x2004, 0x2005, 0x2006, 0x2007, 0x2008, 0x2009, 0x200A, 0x202F, 0x205F];

/// Normalises text for typing: plain `\n` newlines and plain spaces, no control characters other
/// than newline and tab, and no trailing newline (a final Enter would submit most single-line
/// fields).
pub fn prepare(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' | '\t' => out.push(c),
            c if ODD_SPACES.contains(&u32::from(c)) => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    let end = out.trim_end_matches('\n').len();
    out.truncate(end);
    out
}

/// One line of `text` for a list: runs of whitespace (newlines included) become single spaces,
/// and anything past `max_chars` characters is cut off with an ellipsis.
pub fn preview(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut count = 0;
    for word in text.split_whitespace() {
        if count > 0 {
            out.push(' ');
            count += 1;
        }
        for c in word.chars() {
            if count >= max_chars {
                let end = out.trim_end().len();
                out.truncate(end);
                out.push('…');
                return out;
            }
            out.push(c);
            count += 1;
        }
    }
    out
}

/// How many lines `text` has (a trailing newline doesn't start another).
pub fn line_count(text: &str) -> usize {
    text.lines().count().max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepares_newlines_spaces_and_controls() {
        let nbsp = char::from_u32(0xA0).unwrap();
        let thin = char::from_u32(0x2009).unwrap();
        let input = format!("a\r\nb\rc\nd\te{nbsp}f{thin}g\x07h\x7fi\n\n");
        assert_eq!(prepare(&input), "a\nb\nc\nd\te f ghi");
    }

    #[test]
    fn keeps_everything_else() {
        assert_eq!(prepare("Grüße, 日本語 & emoji 🦀"), "Grüße, 日本語 & emoji 🦀");
        assert_eq!(prepare(""), "");
        assert_eq!(prepare("\n\n"), "");
        // C1 controls go too.
        assert_eq!(prepare(&format!("x{}y", char::from_u32(0x85).unwrap())), "xy");
    }

    #[test]
    fn previews_one_line() {
        assert_eq!(preview("  hello\n\n  world  ", 40), "hello world");
        assert_eq!(preview("abcdefghij", 5), "abcde…");
        assert_eq!(preview("abc def", 4), "abc…");
        assert_eq!(preview("", 5), "");
        assert_eq!(line_count("a\nb\n"), 2);
        assert_eq!(line_count(""), 1);
    }
}
