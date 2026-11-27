use regex::bytes::{Regex, RegexBuilder};

use crate::{CaseSensitivity, QuarryError};

/// Maximum UTF-8 byte length accepted for a user-supplied regular expression pattern.
pub const MAX_REGEX_PATTERN_BYTES: usize = 4_096;

/// Maximum compiled NFA/program memory budget passed to `regex::bytes::RegexBuilder::size_limit`.
pub const MAX_REGEX_COMPILED_SIZE_BYTES: usize = 2 * 1024 * 1024;

/// Maximum lazy DFA cache budget passed to `regex::bytes::RegexBuilder::dfa_size_limit`.
pub const MAX_REGEX_DFA_SIZE_BYTES: usize = 2 * 1024 * 1024;

/// Maximum syntax nesting depth passed to `regex::bytes::RegexBuilder::nest_limit`.
pub const MAX_REGEX_NEST_LIMIT: u32 = 64;

/// Maximum number of `FilterOperator::Regex` predicates permitted in a single compound filter
/// query, bounding aggregate compiled-regex and lazy-DFA memory across all rules.
pub const MAX_FILTER_REGEX_PREDICATES: usize = 16;

/// Compiles a bounded `regex::bytes::Regex` for per-field matching.
///
/// Patterns are evaluated in Unicode mode (`.unicode(true)`) with per-field `^`/`$`
/// anchors (`.multi_line(false)`) and explicit pattern-length, compiled-size, DFA-size,
/// and nesting bounds so pathological patterns are rejected during validation instead of
/// consuming unbounded memory or stack depth.
///
/// When `case_sensitivity` is [`CaseSensitivity::Insensitive`], regex matching uses
/// Unicode simple case folding (as opposed to literal mode's ASCII case folding).
pub(crate) fn compile_bounded_regex(
    pattern: &[u8],
    case_sensitivity: CaseSensitivity,
) -> Result<Regex, String> {
    if pattern.is_empty() {
        return Err("regex pattern must not be empty".to_owned());
    }
    if pattern.len() > MAX_REGEX_PATTERN_BYTES {
        return Err(format!(
            "regex pattern exceeds the {MAX_REGEX_PATTERN_BYTES}-byte limit"
        ));
    }
    let pattern_str =
        std::str::from_utf8(pattern).map_err(|_| "regex pattern must be valid UTF-8".to_owned())?;

    RegexBuilder::new(pattern_str)
        .unicode(true)
        .multi_line(false)
        .case_insensitive(case_sensitivity == CaseSensitivity::Insensitive)
        .size_limit(MAX_REGEX_COMPILED_SIZE_BYTES)
        .dfa_size_limit(MAX_REGEX_DFA_SIZE_BYTES)
        .nest_limit(MAX_REGEX_NEST_LIMIT)
        .build()
        .map_err(|error| match error {
            regex::Error::CompiledTooBig(limit) => {
                format!("compiled regular expression exceeds the {limit}-byte size limit")
            }
            other => format!("invalid regular expression: {other}"),
        })
}

/// Validates a regular expression against Quarry's syntax and resource bounds.
pub fn validate_regex(
    pattern: &[u8],
    case_sensitivity: CaseSensitivity,
) -> Result<(), QuarryError> {
    compile_bounded_regex(pattern, case_sensitivity)
        .map(|_| ())
        .map_err(QuarryError::InvalidRegex)
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_REGEX_NEST_LIMIT, MAX_REGEX_PATTERN_BYTES, compile_bounded_regex, validate_regex,
    };
    use crate::{CaseSensitivity, QuarryError};

    #[test]
    fn rejects_empty_oversized_invalid_utf8_syntax_deep_nesting_and_huge_compiled_regex() {
        let err = compile_bounded_regex(b"", CaseSensitivity::Sensitive).unwrap_err();
        assert!(err.contains("must not be empty"), "got: {err}");

        let too_long = vec![b'a'; MAX_REGEX_PATTERN_BYTES + 1];
        let err = compile_bounded_regex(&too_long, CaseSensitivity::Sensitive).unwrap_err();
        assert!(err.contains("4096-byte limit"), "got: {err}");

        let err = compile_bounded_regex(b"\xff\xfe", CaseSensitivity::Sensitive).unwrap_err();
        assert!(err.contains("valid UTF-8"), "got: {err}");

        let err = compile_bounded_regex(b"[unclosed", CaseSensitivity::Sensitive).unwrap_err();
        assert!(err.contains("invalid regular expression"), "got: {err}");

        let depth = (MAX_REGEX_NEST_LIMIT as usize) + 16;
        let mut deeply_nested = String::with_capacity(depth * 2 + 1);
        for _ in 0..depth {
            deeply_nested.push('(');
        }
        deeply_nested.push('a');
        for _ in 0..depth {
            deeply_nested.push(')');
        }
        let err = compile_bounded_regex(deeply_nested.as_bytes(), CaseSensitivity::Sensitive)
            .unwrap_err();
        assert!(err.contains("invalid regular expression"), "got: {err}");

        // Large repetition explodes compiled NFA size beyond MAX_REGEX_COMPILED_SIZE_BYTES.
        let err = compile_bounded_regex(b"a{1000000}", CaseSensitivity::Sensitive).unwrap_err();
        assert!(err.contains("size limit"), "got: {err}");

        assert!(matches!(
            validate_regex(b"[unclosed", CaseSensitivity::Sensitive),
            Err(QuarryError::InvalidRegex(_))
        ));
    }

    #[test]
    fn anchors_are_per_field_and_unicode_case_folding_applies_without_lossy_utf8() {
        let anchored = compile_bounded_regex(b"^bar$", CaseSensitivity::Sensitive).unwrap();
        assert!(anchored.is_match(b"bar"));
        assert!(!anchored.is_match(b"foo\nbar\nbaz"));

        let unicode_ci =
            compile_bounded_regex(b"^caf\xc3\xa9$", CaseSensitivity::Insensitive).unwrap();
        assert!(unicode_ci.is_match("CAFÉ".as_bytes()));
        assert!(unicode_ci.is_match("café".as_bytes()));

        let unicode_cs =
            compile_bounded_regex(b"^caf\xc3\xa9$", CaseSensitivity::Sensitive).unwrap();
        assert!(!unicode_cs.is_match("CAFÉ".as_bytes()));
        assert!(unicode_cs.is_match("café".as_bytes()));

        // Raw invalid UTF-8 bytes in a UTF-8 file are matched safely without lossy replacement.
        let raw_literal = compile_bounded_regex(b"needle", CaseSensitivity::Sensitive).unwrap();
        assert!(raw_literal.is_match(b"\xffneedle\xfe"));
        // U+FFFD replacement character is NOT synthesized for invalid bytes.
        let fffd =
            compile_bounded_regex("\u{FFFD}".as_bytes(), CaseSensitivity::Sensitive).unwrap();
        assert!(!fffd.is_match(b"\xff\xfe"));
    }
}
