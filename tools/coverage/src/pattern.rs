//! A minimal regex subset for Bazel's `--filter_sources` patterns.
//!
//! Bazel's `collect_coverage.sh` invokes the LCOV merger with a fixed set of
//! filter patterns (`/usr/bin/.+`, `/usr/lib/.+`, `/usr/include.+`,
//! `/Applications/.+`), and users may add their own via custom rules. The
//! built-in CoverageOutputGenerator treats these as full-match Java regexes.
//! This module implements the small subset those patterns actually need —
//! literals, `.`, character classes, `*`/`+`/`?` quantifiers, and `\`
//! escapes — with full-string anchoring, avoiding a third-party regex crate.
//!
//! Patterns using syntax outside the subset (alternation, groups, `{n,m}`
//! repetition, anchors) fail to compile; the caller warns and skips the
//! filter rather than aborting the coverage action.

#[derive(Debug, Clone, PartialEq)]
enum Elem {
    /// A literal character.
    Char(char),
    /// `.` — any single character.
    Any,
    /// `[...]` — a character class; `(negated, ranges)`.
    Class(bool, Vec<(char, char)>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Quant {
    One,
    ZeroOrOne,
    ZeroOrMore,
    OneOrMore,
}

/// A compiled pattern: a sequence of (element, quantifier) pairs matched
/// against the full input string.
#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    terms: Vec<(Elem, Quant)>,
}

impl Pattern {
    /// Compile `source`, or return a description of the unsupported syntax.
    pub fn compile(source: &str) -> Result<Pattern, String> {
        let mut terms: Vec<(Elem, Quant)> = Vec::new();
        let mut chars = source.chars().peekable();
        while let Some(c) = chars.next() {
            let elem = match c {
                '.' => Elem::Any,
                '\\' => match chars.next() {
                    Some(escaped @ ('.' | '\\' | '*' | '+' | '?' | '[' | ']' | '/' | '-')) => {
                        Elem::Char(escaped)
                    }
                    Some(other) => {
                        return Err(format!("unsupported escape '\\{other}' in '{source}'"))
                    }
                    None => return Err(format!("dangling '\\' in '{source}'")),
                },
                '[' => {
                    let negated = chars.peek() == Some(&'^');
                    if negated {
                        chars.next();
                    }
                    let mut ranges = Vec::new();
                    loop {
                        match chars.next() {
                            None => return Err(format!("unterminated '[' in '{source}'")),
                            Some(']') if !ranges.is_empty() => break,
                            Some(lo) => {
                                if chars.peek() == Some(&'-') {
                                    chars.next();
                                    match chars.next() {
                                        Some(hi) if hi != ']' => ranges.push((lo, hi)),
                                        _ => {
                                            return Err(format!(
                                                "unterminated range in class in '{source}'"
                                            ))
                                        }
                                    }
                                } else {
                                    ranges.push((lo, lo));
                                }
                            }
                        }
                    }
                    Elem::Class(negated, ranges)
                }
                '*' | '+' | '?' => {
                    return Err(format!(
                        "quantifier '{c}' with nothing to repeat in '{source}'"
                    ))
                }
                '(' | ')' | '|' | '{' | '}' | '^' | '$' => {
                    return Err(format!("unsupported regex syntax '{c}' in '{source}'"))
                }
                other => Elem::Char(other),
            };
            let quant = match chars.peek() {
                Some('*') => {
                    chars.next();
                    Quant::ZeroOrMore
                }
                Some('+') => {
                    chars.next();
                    Quant::OneOrMore
                }
                Some('?') => {
                    chars.next();
                    Quant::ZeroOrOne
                }
                _ => Quant::One,
            };
            terms.push((elem, quant));
        }
        Ok(Pattern { terms })
    }

    /// Full-string match (like Java's `Matcher.matches`).
    pub fn matches(&self, input: &str) -> bool {
        let chars: Vec<char> = input.chars().collect();
        matches_here(&self.terms, &chars, 0)
    }
}

fn elem_matches(elem: &Elem, c: char) -> bool {
    match elem {
        Elem::Char(lit) => *lit == c,
        Elem::Any => true,
        Elem::Class(negated, ranges) => {
            let inside = ranges.iter().any(|(lo, hi)| *lo <= c && c <= *hi);
            inside != *negated
        }
    }
}

/// Backtracking matcher: does `terms` consume exactly `input[pos..]`?
fn matches_here(terms: &[(Elem, Quant)], input: &[char], pos: usize) -> bool {
    let Some(((elem, quant), rest)) = terms.split_first() else {
        return pos == input.len();
    };
    match quant {
        Quant::One => {
            pos < input.len()
                && elem_matches(elem, input[pos])
                && matches_here(rest, input, pos + 1)
        }
        Quant::ZeroOrOne => {
            (pos < input.len()
                && elem_matches(elem, input[pos])
                && matches_here(rest, input, pos + 1))
                || matches_here(rest, input, pos)
        }
        Quant::ZeroOrMore | Quant::OneOrMore => {
            // Greedily consume the longest run, then backtrack one character
            // at a time until the rest of the pattern matches.
            let mut end = pos;
            while end < input.len() && elem_matches(elem, input[end]) {
                end += 1;
            }
            let min_end = if *quant == Quant::OneOrMore {
                pos + 1
            } else {
                pos
            };
            loop {
                if end < min_end {
                    return false;
                }
                if matches_here(rest, input, end) {
                    return true;
                }
                if end == 0 {
                    return false;
                }
                end -= 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pattern: &str, input: &str) -> bool {
        Pattern::compile(pattern).unwrap().matches(input)
    }

    #[test]
    fn bazels_default_filters_behave_like_full_match_regexes() {
        assert!(matches("/usr/bin/.+", "/usr/bin/gcov"));
        assert!(!matches("/usr/bin/.+", "/usr/bin/"));
        assert!(!matches("/usr/bin/.+", "tools/usr/bin/x"));
        assert!(matches("/usr/include.+", "/usr/include/stdio.h"));
        assert!(matches("/Applications/.+", "/Applications/Xcode.app/x.h"));
        assert!(!matches("/Applications/.+", "cli/src/main/kotlin/Main.kt"));
    }

    #[test]
    fn literals_dots_and_quantifiers() {
        assert!(matches("a.c", "abc"));
        assert!(!matches("a.c", "abbc"));
        assert!(matches("ab*c", "ac"));
        assert!(matches("ab*c", "abbbc"));
        assert!(matches("ab?c", "ac"));
        assert!(matches("ab?c", "abc"));
        assert!(!matches("ab?c", "abbc"));
        assert!(matches(".*third_party.*", "some/third_party/lib.cc"));
        assert!(matches("a\\.c", "a.c"));
        assert!(!matches("a\\.c", "abc"));
    }

    #[test]
    fn character_classes() {
        assert!(matches("[a-c]+x", "abcx"));
        assert!(!matches("[a-c]+x", "adx"));
        assert!(matches("[^/]+\\.go", "sample.go"));
        assert!(!matches("[^/]+\\.go", "dir/sample.go"));
        // A ']' in first position inside a class is a literal, as in POSIX.
        assert!(matches("[]]", "]"));
    }

    #[test]
    fn backtracking_terminates_and_matches_greedily() {
        assert!(matches(".+.go", "sample.go"));
        assert!(matches(".*.*x", "aaax"));
        assert!(!matches(".+x", ""));
        assert!(!matches(".+", ""));
        assert!(matches(".*", ""));
    }

    #[test]
    fn unsupported_syntax_is_rejected_not_misparsed() {
        for bad in [
            "a|b", "(ab)+", "a{2}", "^a", "a$", "\\d+", "a\\", "[abc", "+a", "[a-]x",
        ] {
            assert!(Pattern::compile(bad).is_err(), "expected error for {bad}");
        }
    }
}
