use memchr::memmem::Finder;
use memchr::{memchr_iter, memchr2_iter};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaseSensitivity {
    Insensitive,
    Sensitive,
}

pub(crate) struct ByteMatcher<'a> {
    needle: &'a [u8],
    case_sensitivity: CaseSensitivity,
    finder: Finder<'a>,
}

impl<'a> ByteMatcher<'a> {
    pub(crate) fn new(needle: &'a [u8], case_sensitivity: CaseSensitivity) -> Self {
        Self {
            needle,
            case_sensitivity,
            finder: Finder::new(needle),
        }
    }

    pub(crate) fn find(&self, haystack: &[u8]) -> Option<usize> {
        if self.case_sensitivity == CaseSensitivity::Sensitive {
            return self.finder.find(haystack);
        }
        if self.needle.is_empty() {
            return Some(0);
        }
        if self.needle.len() > haystack.len() {
            return None;
        }

        let first = self.needle[0];
        if first.is_ascii_alphabetic() {
            memchr2_iter(
                first.to_ascii_lowercase(),
                first.to_ascii_uppercase(),
                haystack,
            )
            .find(|&position| self.matches_at(haystack, position))
        } else {
            memchr_iter(first, haystack).find(|&position| self.matches_at(haystack, position))
        }
    }

    pub(crate) fn equals(&self, value: &[u8]) -> bool {
        match self.case_sensitivity {
            CaseSensitivity::Insensitive => value.eq_ignore_ascii_case(self.needle),
            CaseSensitivity::Sensitive => value == self.needle,
        }
    }

    fn matches_at(&self, haystack: &[u8], position: usize) -> bool {
        haystack
            .get(position..position.saturating_add(self.needle.len()))
            .is_some_and(|value| value.eq_ignore_ascii_case(self.needle))
    }
}

#[cfg(test)]
mod tests {
    use super::{ByteMatcher, CaseSensitivity};

    #[test]
    fn matcher_can_ignore_or_match_ascii_case_without_changing_offsets() {
        let insensitive = ByteMatcher::new(b"tx", CaseSensitivity::Insensitive);
        let sensitive = ByteMatcher::new(b"tx", CaseSensitivity::Sensitive);

        assert_eq!(insensitive.find(b"State: TX"), Some(7));
        assert!(insensitive.equals(b"Tx"));
        assert_eq!(sensitive.find(b"State: TX"), None);
        assert!(!sensitive.equals(b"Tx"));
    }
}
