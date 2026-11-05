//! Minimal Markdown for messages: hidden markers, styles per character, emoji and wrapping.

use std::ops::Range;

use unicode_width::UnicodeWidthChar;

/// The styles a character can carry, combined as bits.
pub type Flags = u8;
/// Bold text.
pub const BOLD: Flags = 1;
/// Italic text.
pub const ITALIC: Flags = 2;
/// Struck-through text.
pub const STRIKE: Flags = 4;
/// Code, which nothing inside is interpreted.
pub const CODE: Flags = 8;
/// A quoted line.
pub const QUOTE: Flags = 16;
/// A web address, or the text of a link.
pub const LINK: Flags = 32;
/// Secondary text such as a reply marker or a link target.
pub const MUTED: Flags = 64;

/// Characters with a set of styles each.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Text {
    pub chars: Vec<char>,
    pub flags: Vec<Flags>,
}

impl Text {
    /// Adds a string with the given styles.
    pub fn push_str(&mut self, text: &str, flags: Flags) {
        for c in text.chars() {
            self.chars.push(c);
            self.flags.push(flags);
        }
    }

    /// Adds another text after this one.
    pub fn append(&mut self, other: Self) {
        self.chars.extend(other.chars);
        self.flags.extend(other.flags);
    }

    /// The characters as a string.
    #[cfg(test)]
    pub fn to_plain(&self) -> String {
        self.chars.iter().collect()
    }
}

/// Marks the colored form of an emoji such as the red heart, which terminals draw one or two cells wide.
pub const COLORED: char = '\u{fe0f}';

/// Replaces `:shortcode:` with the emoji it names; unknown names stay as they are.
pub fn emoji(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(':') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let len = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-')))
            .unwrap_or(after.len());
        let found = (len > 0 && after[len..].starts_with(':'))
            .then(|| emojis::get_by_shortcode(&after[..len]))
            .flatten();
        match found {
            Some(e) => {
                out.push_str(e.as_str());
                rest = &after[len + 1..];
            }
            None => {
                out.push(':');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn word(c: char) -> bool {
    c.is_alphanumeric()
}

fn find(chars: &[char], from: usize, delimiter: &[char], boundary: bool) -> Option<usize> {
    let mut j = from;
    while j + delimiter.len() <= chars.len() {
        if chars[j] == '\\' {
            j += 2;
            continue;
        }
        let closes = chars[j..j + delimiter.len()] == *delimiter
            && j > from
            && !chars[j - 1].is_whitespace()
            && (!boundary || !chars.get(j + delimiter.len()).is_some_and(|&c| word(c)));
        if closes {
            return Some(j);
        }
        j += 1;
    }
    None
}

fn inline(chars: &[char], base: Flags, out: &mut Text) {
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let before_word = i > 0 && word(chars[i - 1]);
        let next = chars.get(i + 1).copied();
        let spaced = |k: usize| chars.get(k).is_none_or(|c| c.is_whitespace());
        if c == '\\' && next.is_some_and(|n| "\\`*_~[]()#>".contains(n)) {
            out.chars.push(chars[i + 1]);
            out.flags.push(base);
            i += 2;
        } else if c == '`' {
            match chars[i + 1..]
                .iter()
                .position(|&x| x == '`')
                .filter(|&p| p > 0)
            {
                Some(p) => {
                    for &x in &chars[i + 1..i + 1 + p] {
                        out.chars.push(x);
                        out.flags.push(base | CODE);
                    }
                    i += p + 2;
                }
                None => i = literal(chars, i, base, out),
            }
        } else if let Some((delimiter, flag, boundary)) = delimiter(chars, i, before_word) {
            let start = i + delimiter.len();
            let closing = (!spaced(start))
                .then(|| find(chars, start, delimiter, boundary))
                .flatten();
            match closing {
                Some(end) => {
                    inline(&chars[start..end], base | flag, out);
                    i = end + delimiter.len();
                }
                None => i = literal(chars, i, base, out),
            }
        } else if c == '[' {
            i = link(chars, i, base, out);
        } else if (c == 'h') && !before_word && url_len(&chars[i..]) > 0 {
            let len = url_len(&chars[i..]);
            for &x in &chars[i..i + len] {
                out.chars.push(x);
                out.flags.push(base | LINK);
            }
            i += len;
        } else {
            i = literal(chars, i, base, out);
        }
    }
}

fn literal(chars: &[char], i: usize, base: Flags, out: &mut Text) -> usize {
    out.chars.push(chars[i]);
    out.flags.push(base);
    i + 1
}

fn delimiter(
    chars: &[char],
    i: usize,
    before_word: bool,
) -> Option<(&'static [char], Flags, bool)> {
    let pair = chars.get(i + 1).copied();
    match (chars[i], pair) {
        ('*', Some('*')) => Some((&['*', '*'], BOLD, false)),
        ('_', Some('_')) if !before_word => Some((&['_', '_'], BOLD, true)),
        ('~', Some('~')) => Some((&['~', '~'], STRIKE, false)),
        ('*', _) if !before_word => Some((&['*'], ITALIC, true)),
        ('_', _) if !before_word => Some((&['_'], ITALIC, true)),
        _ => None,
    }
}

/// How many characters of a web address start `chars`, or 0.
pub fn url_len(chars: &[char]) -> usize {
    let text: String = chars.iter().take(8).collect();
    if !(text.starts_with("http://") || text.starts_with("https://")) {
        return 0;
    }
    let mut end = chars
        .iter()
        .position(|c| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`'))
        .unwrap_or(chars.len());
    while end > 0 {
        let last = chars[end - 1];
        let opened = chars[..end].iter().filter(|&&c| c == '(').count();
        let closed = chars[..end].iter().filter(|&&c| c == ')').count();
        if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '\'')
            || (last == ')' && closed > opened)
        {
            end -= 1;
        } else {
            break;
        }
    }
    if end > text.find("//").map_or(0, |p| p + 3) {
        end
    } else {
        0
    }
}

fn link(chars: &[char], i: usize, base: Flags, out: &mut Text) -> usize {
    let close = chars[i + 1..].iter().position(|&c| c == ']');
    let target = close
        .filter(|&p| chars.get(i + 1 + p + 1) == Some(&'('))
        .and_then(|p| {
            let open = i + 1 + p + 2;
            let end = chars[open..].iter().position(|&c| c == ')')?;
            Some((p, open, open + end))
        });
    let Some((p, open, end)) = target.filter(|&(p, ..)| p > 0) else {
        return literal(chars, i, base, out);
    };
    inline(&chars[i + 1..i + 1 + p], base | LINK, out);
    let url: String = chars[open..end].iter().collect();
    if chars[i + 1..i + 1 + p].iter().collect::<String>() != url {
        out.push_str(&format!(" ({url})"), base | MUTED);
    }
    end + 1
}

/// Turns a message into styled characters: Markdown markers hidden, emoji replaced outside code.
pub fn render(source: &str) -> Text {
    let mut out = Text::default();
    let mut fenced = false;
    let source = source.replace('\r', "");
    let lines: Vec<&str> = source.split('\n').collect();
    for (n, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        if fenced {
            out.push_str(line, CODE);
        } else if let Some(rest) = heading(&chars) {
            inline(rest, BOLD, &mut out);
        } else if let Some(rest) = line.strip_prefix("> ") {
            out.push_str("│ ", QUOTE);
            inline(&rest.chars().collect::<Vec<_>>(), QUOTE, &mut out);
        } else {
            inline(&chars, 0, &mut out);
        }
        if n + 1 < lines.len() {
            out.chars.push('\n');
            out.flags.push(0);
        }
    }
    while out.chars.last() == Some(&'\n') {
        out.chars.pop();
        out.flags.pop();
    }
    with_emoji(out)
}

fn heading(chars: &[char]) -> Option<&[char]> {
    let hashes = chars.iter().take_while(|&&c| c == '#').count();
    (1..=6).contains(&hashes).then_some(())?;
    (chars.get(hashes) == Some(&' ')).then(|| &chars[hashes + 1..])
}

fn with_emoji(text: Text) -> Text {
    let mut out = Text::default();
    let mut start = 0;
    while start < text.chars.len() {
        let flags = text.flags[start];
        let end = (start..text.chars.len())
            .find(|&k| text.flags[k] != flags)
            .unwrap_or(text.chars.len());
        let run: String = text.chars[start..end].iter().collect();
        out.push_str(&if flags & CODE != 0 { run } else { emoji(&run) }, flags);
        start = end;
    }
    out
}

fn cells(c: char) -> usize {
    c.width().unwrap_or(0)
}

/// Splits characters into lines of at most `width` cells, at spaces when it can; line breaks
/// in the text are kept and the spaces at a break are dropped.
pub fn wrap(chars: &[char], width: usize) -> Vec<Range<usize>> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut start = 0;
    loop {
        let end = chars[start..]
            .iter()
            .position(|&c| c == '\n')
            .map_or(chars.len(), |p| start + p);
        wrap_paragraph(chars, start, end, width, &mut lines);
        if end >= chars.len() {
            return lines;
        }
        start = end + 1;
    }
}

fn wrap_paragraph(
    chars: &[char],
    start: usize,
    end: usize,
    width: usize,
    lines: &mut Vec<Range<usize>>,
) {
    let (mut line_start, mut used, mut last_end, mut has_word) = (start, 0, start, false);
    let mut i = start;
    while i < end {
        let gap_start = i;
        while i < end && chars[i] == ' ' {
            i += 1;
        }
        let gap = i - gap_start;
        let word_start = i;
        while i < end && chars[i] != ' ' {
            i += 1;
        }
        if word_start == i {
            break;
        }
        let word_width: usize = chars[word_start..i].iter().map(|&c| cells(c)).sum();
        if has_word && used + gap + word_width > width {
            lines.push(line_start..last_end);
            (line_start, used, has_word) = (word_start, 0, false);
        }
        if has_word || line_start == start {
            used += gap;
        }
        if used + word_width <= width {
            used += word_width;
        } else {
            for (k, &c) in chars.iter().enumerate().take(i).skip(word_start) {
                let w = cells(c);
                if used + w > width && k > line_start {
                    lines.push(line_start..k);
                    (line_start, used) = (k, 0);
                }
                used += w;
            }
        }
        (has_word, last_end) = (true, i);
    }
    lines.push(line_start..last_end.max(line_start));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(source: &str) -> String {
        render(source).to_plain()
    }

    fn flags_of(source: &str, wanted: &str) -> Vec<Flags> {
        let text = render(source);
        let all = text.to_plain();
        let at = all.find(wanted).unwrap();
        let from = all[..at].chars().count();
        text.flags[from..from + wanted.chars().count()].to_vec()
    }

    #[test]
    fn emphasis_markers_are_hidden_and_styled() {
        assert_eq!(plain("a **bold** b"), "a bold b");
        assert_eq!(flags_of("a **bold** b", "bold"), [BOLD; 4]);
        assert_eq!(flags_of("an *italic* word", "italic"), [ITALIC; 6]);
        assert_eq!(flags_of("an _italic_ word", "italic"), [ITALIC; 6]);
        assert_eq!(flags_of("a __bold__ b", "bold"), [BOLD; 4]);
        assert_eq!(flags_of("a ~~gone~~ b", "gone"), [STRIKE; 4]);
        assert_eq!(flags_of("**a _b_ c**", "b"), [BOLD | ITALIC]);
        assert_eq!(flags_of("**a _b_ c**", "a "), [BOLD; 2]);
    }

    #[test]
    fn things_that_look_like_markers_but_are_not_stay_as_typed() {
        for text in [
            "snake_case_name",
            "2*3*4",
            "a * b * c",
            "5 ** 2",
            "**unclosed",
            "_x",
            "~~",
            "a_b_",
            "x *y",
            "100%",
        ] {
            assert_eq!(plain(text), text, "{text}");
            assert!(render(text).flags.iter().all(|&f| f == 0), "{text}");
        }
    }

    #[test]
    fn code_is_protected_from_markers_and_emoji() {
        assert_eq!(
            plain("run `a **b** :tada:` now :tada:"),
            "run a **b** :tada: now 🎉"
        );
        assert_eq!(flags_of("run `x_y` now", "x_y"), [CODE; 3]);
        assert_eq!(
            plain("```rust\nlet a = **b**;\n```\nafter"),
            "let a = **b**;\nafter"
        );
        assert_eq!(flags_of("```\nfn :tada:\n```", "fn :tada:"), [CODE; 9]);
        assert_eq!(plain("`unclosed"), "`unclosed");
    }

    #[test]
    fn headings_quotes_and_escapes() {
        assert_eq!(plain("# Title\ntext"), "Title\ntext");
        assert_eq!(flags_of("## Title", "Title"), [BOLD; 5]);
        assert_eq!(plain("#nospace"), "#nospace");
        assert_eq!(plain("> quoted *x*"), "│ quoted x");
        assert_eq!(flags_of("> quoted", "quoted"), [QUOTE; 6]);
        assert_eq!(plain(r"\*not italic\*"), "*not italic*");
        assert!(render(r"\*not italic\*").flags.iter().all(|&f| f == 0));
    }

    #[test]
    fn links_are_underlined_and_their_target_stays_visible() {
        assert_eq!(
            plain("see [the docs](https://x.org/a) now"),
            "see the docs (https://x.org/a) now"
        );
        assert_eq!(
            flags_of("see [the docs](https://x.org/a)", "the docs"),
            [LINK; 8]
        );
        assert_eq!(
            flags_of("see [the docs](https://x.org/a)", "(https://x.org/a)")[0],
            MUTED
        );
        assert_eq!(
            flags_of("go https://x.org/a.", "https://x.org/a"),
            [LINK; 15]
        );
        assert_eq!(plain("(see https://x.org/a)"), "(see https://x.org/a)");
        assert_eq!(flags_of("(see https://x.org/a)", ")"), [0]);
        assert_eq!(plain("[x]"), "[x]");
        assert_eq!(plain("[](u)"), "[](u)");
    }

    #[test]
    fn shortcodes_become_emoji() {
        assert_eq!(emoji("well done :tada:!"), "well done 🎉!");
        assert_eq!(
            emoji(":+1: :nope_nope: 12:30:45"),
            "👍 :nope_nope: 12:30:45"
        );
        assert_eq!(emoji("a::b"), "a::b");
        assert_eq!(plain("**:tada:**"), "🎉");
    }

    fn lines(text: &str, width: usize) -> Vec<String> {
        let chars: Vec<char> = text.chars().collect();
        wrap(&chars, width)
            .into_iter()
            .map(|r| chars[r].iter().collect())
            .collect()
    }

    #[test]
    fn wrapping_breaks_at_spaces_cuts_long_words_and_keeps_line_breaks() {
        assert_eq!(lines("one two three", 7), ["one two", "three"]);
        assert_eq!(lines("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(lines("a\n\nb", 5), ["a", "", "b"]);
        assert_eq!(lines("日本語 日本", 6), ["日本語", "日本"]);
        assert_eq!(lines("", 5), [""]);
        assert_eq!(
            lines("  indented words here", 12),
            ["  indented", "words here"]
        );
        assert_eq!(lines("a  b", 10), ["a  b"]);
        assert_eq!(lines("ab cdefghij", 4), ["ab", "cdef", "ghij"]);
        assert_eq!(lines("x", 0), ["x"]);
    }

    #[test]
    fn a_style_survives_wrapping() {
        let text = render("**one two three**");
        let ranges = wrap(&text.chars, 7);
        assert_eq!(ranges.len(), 2);
        assert!(text.flags[ranges[1].clone()].iter().all(|&f| f == BOLD));
    }
}
