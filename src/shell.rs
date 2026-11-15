//! Command lines split the way GLib's `g_shell_parse_argv` splits them, as
//! IBus does for the commands of its component files: words are separated
//! by blanks, single and double quotes group them, backslashes escape, and
//! a `#` at the start of a word begins a comment. There are no variables,
//! globs, pipes or other shell features.

/// The arguments of `command_line`, or `None` where GLib reports an error
/// (an unmatched quote, a trailing backslash, or no words at all).
pub fn parse_argv(command_line: &str) -> Option<Vec<String>> {
    let tokens = tokenize(command_line)?;
    if tokens.is_empty() {
        return None;
    }
    tokens.iter().map(|token| unquote(token)).collect()
}

/// Split into words, keeping their quotes and escapes for `unquote`.
fn tokenize(command_line: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut token: Option<String> = None;
    let mut quote: Option<char> = None;
    // Whether the previous character was an odd-numbered backslash, which
    // escapes a double quote inside double quotes.
    let mut escaped = false;
    let mut previous: Option<char> = None;
    let mut chars = command_line.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some('\\') => {
                // A backslash-newline outside quotes disappears.
                if c != '\n' {
                    let t = token.get_or_insert_with(String::new);
                    t.push('\\');
                    t.push(c);
                }
                quote = None;
            }
            Some('#') => {
                // A comment runs to the end of the line.
                let mut ended = c == '\n';
                if !ended {
                    for c in chars.by_ref() {
                        if c == '\n' {
                            ended = true;
                            break;
                        }
                    }
                }
                quote = None;
                if !ended {
                    break;
                }
                previous = Some('\n');
                escaped = false;
                continue;
            }
            Some(q) => {
                if c == q && !(q == '"' && escaped) {
                    quote = None;
                }
                token.get_or_insert_with(String::new).push(c);
            }
            None => match c {
                '\n' => tokens.extend(token.take()),
                ' ' | '\t' => {
                    if token.as_ref().is_some_and(|t| !t.is_empty()) {
                        tokens.extend(token.take());
                    }
                }
                '\'' | '"' => {
                    token.get_or_insert_with(String::new).push(c);
                    quote = Some(c);
                }
                '\\' => quote = Some('\\'),
                '#' if matches!(previous, None | Some(' ' | '\n')) => quote = Some('#'),
                _ => token.get_or_insert_with(String::new).push(c),
            },
        }
        escaped = c == '\\' && !escaped;
        previous = Some(c);
    }
    tokens.extend(token.take());
    if quote.is_some() {
        return None;
    }
    Some(tokens)
}

/// Remove the quotes and escapes of one word, as `g_shell_unquote` does.
fn unquote(token: &str) -> Option<String> {
    let mut word = String::new();
    let mut chars = token.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                // Any character can be escaped; a backslash-newline goes.
                if let Some(next) = chars.next() {
                    if next != '\n' {
                        word.push(next);
                    }
                }
            }
            '\'' => loop {
                match chars.next()? {
                    '\'' => break,
                    c => word.push(c),
                }
            },
            '"' => loop {
                match chars.next()? {
                    '"' => break,
                    '\\' => match chars.next()? {
                        c @ ('"' | '\\' | '`' | '$' | '\n') => word.push(c),
                        c => {
                            word.push('\\');
                            word.push(c);
                        }
                    },
                    c => word.push(c),
                }
            },
            c => word.push(c),
        }
    }
    Some(word)
}

#[cfg(test)]
mod tests {
    use super::parse_argv;

    fn argv(command_line: &str) -> Option<Vec<String>> {
        parse_argv(command_line)
    }

    fn words(words: &[&str]) -> Option<Vec<String>> {
        Some(words.iter().map(|w| w.to_string()).collect())
    }

    #[test]
    fn words_are_split_at_blanks() {
        assert_eq!(
            argv("/usr/libexec/ibus-engine-m17n  --xml\t-v\n x"),
            words(&["/usr/libexec/ibus-engine-m17n", "--xml", "-v", "x"])
        );
    }

    #[test]
    fn quotes_and_backslashes_group_words() {
        assert_eq!(
            argv(r#"cmd 'one two' "three\"four" "five'six" seven\ eight "a\b" 'c\d' x""y"#),
            words(&[
                "cmd",
                "one two",
                "three\"four",
                "five'six",
                "seven eight",
                "a\\b",
                "c\\d",
                "xy"
            ])
        );
    }

    #[test]
    fn shell_features_are_literal() {
        assert_eq!(
            argv("cmd $HOME ~ * | ; > out `x`"),
            words(&["cmd", "$HOME", "~", "*", "|", ";", ">", "out", "`x`"])
        );
    }

    #[test]
    fn comments_start_at_word_boundaries() {
        assert_eq!(argv("cmd a#b # comment\nc"), words(&["cmd", "a#b", "c"]));
        assert_eq!(argv("cmd # comment"), words(&["cmd"]));
    }

    #[test]
    fn errors_give_no_arguments() {
        for command_line in ["", "  \n ", "cmd 'open", "cmd \"open", "cmd \\", "# only"] {
            assert_eq!(argv(command_line), None, "{command_line:?}");
        }
    }
}
