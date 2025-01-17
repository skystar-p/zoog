use std::borrow::Cow;

use thiserror::Error;

/// The escape character
const ESCAPE_CHAR: char = '\\';

/// Characters which are escaped by tag processing tools
const ESCAPED_CHARS: [char; 4] = ['\0', '\n', '\r', '\\'];

/// Wraps an iterator to apply `vorbiscomemnt`-style character escaping
#[derive(Debug)]
struct EscapingIterator<I> {
    inner: I,
    delayed: Option<char>,
}

impl<I> EscapingIterator<I> {
    pub fn new(inner: I) -> EscapingIterator<I> { EscapingIterator { inner, delayed: None } }
}

impl<I> Iterator for EscapingIterator<I>
where
    I: Iterator<Item = char>,
{
    type Item = char;

    fn next(&mut self) -> Option<char> {
        if self.delayed.is_none() {
            self.inner.next().map(|c| {
                self.delayed = match c {
                    '\0' => Some('0'),
                    '\n' => Some('n'),
                    '\r' => Some('r'),
                    '\\' => Some('\\'),
                    _ => None,
                };
                if self.delayed.is_some() {
                    ESCAPE_CHAR
                } else {
                    c
                }
            })
        } else {
            self.delayed.take()
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) { (self.inner.size_hint().0, None) }
}

/// Escapes a string slice using `vorbiscomment`-style escaping
pub fn escape_str(value: &str) -> Cow<str> {
    if value.contains(ESCAPED_CHARS) {
        EscapingIterator::new(value.chars()).collect()
    } else {
        value.into()
    }
}

/// Error type for failure to decode an escaped string
#[derive(Debug, Error)]
pub enum EscapeDecodeError {
    /// The string ended with a backslash
    #[error("Trailing backslash in escaped string")]
    TrailingBackslash,

    /// An invalid character followed a backslash in an escaped string
    #[error("Invalid character following backslash in escaped string: `{0}`")]
    InvalidEscape(char),
}

/// Unescapes a string slice using `vorbiscomment`-style escaping
pub fn unescape_str(value: &str) -> Result<Cow<str>, EscapeDecodeError> {
    if !value.contains(ESCAPE_CHAR) {
        return Ok(value.into());
    }
    let mut result = String::with_capacity(value.len());
    let mut is_escape = false;
    for c in value.chars() {
        if is_escape {
            result.push(match c {
                '0' => '\0',
                'n' => '\n',
                'r' => '\r',
                '\\' => '\\',
                _ => return Err(EscapeDecodeError::InvalidEscape(c)),
            });
            is_escape = false;
        } else if c == ESCAPE_CHAR {
            is_escape = true;
        } else {
            result.push(c);
        }
    }

    if is_escape {
        Err(EscapeDecodeError::TrailingBackslash)
    } else {
        Ok(result.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_safe(value: &str) -> bool {
        // Escaped strings may still contain the escape character so we don't include it
        !value.contains(['\0', '\n', '\r'])
    }

    // So we don't have to use unstable features. Function names chosen not to
    // conflict in the case that this becomes stable.
    // https://github.com/rust-lang/rust/issues/65143
    trait IntrospectCowBorrow {
        fn is_cow_owned(&self) -> bool;
        fn is_cow_borrowed(&self) -> bool;
    }

    impl<'a, T> IntrospectCowBorrow for Cow<'a, T>
    where
        T: ToOwned + ?Sized,
    {
        fn is_cow_owned(&self) -> bool {
            if let Cow::Owned(_) = self {
                true
            } else {
                false
            }
        }

        fn is_cow_borrowed(&self) -> bool { !self.is_cow_owned() }
    }

    #[test]
    fn escape_non_special() {
        let original = "The quick brown fox jumps over the lazy dog";
        assert!(is_safe(original));

        let escaped = escape_str(original);
        assert!(is_safe(&escaped));
        assert!(escaped.is_cow_borrowed());
        assert_eq!(original, escaped);

        let unescaped = unescape_str(&escaped).expect("Unable to unescape string");
        assert!(unescaped.is_cow_borrowed());
        assert_eq!(original, unescaped);
    }

    #[test]
    fn escape_special() {
        let original = "\0\n\r\\";
        assert!(!is_safe(original));

        let escaped = escape_str(original);
        assert!(is_safe(&escaped));
        assert!(escaped.is_cow_owned());
        assert_eq!(escaped, "\\0\\n\\r\\\\");

        let unescaped = unescape_str(&escaped).expect("Unable to reverse escaping");
        assert!(unescaped.is_cow_owned());
        assert_eq!(original, unescaped);
    }

    #[test]
    fn escaping_special_by_char() {
        // Pick up bugs in detecting if strings need to be escaped by testing each
        // escaped character indivually
        for c in &ESCAPED_CHARS {
            let original = c.to_string();

            let escaped = escape_str(&original);
            assert_eq!(escaped.len(), 2);
            assert!(is_safe(&escaped));
            assert!(escaped.is_cow_owned());

            let unescaped = unescape_str(&escaped).expect("Unable to reverse escaping");
            assert!(unescaped.is_cow_owned());
            assert_eq!(original, unescaped);
        }
    }
}
