use crate::{MAX_TRANSFORMATION_COLUMNS, QuarryError};

/// A value-preserving-schema cleanup applied to existing fields only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CleanupOperation {
    Trim,
    Uppercase,
    Lowercase,
    FillEmpty(Vec<u8>),
}

impl CleanupOperation {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Trim => "Trim whitespace",
            Self::Uppercase => "Uppercase",
            Self::Lowercase => "Lowercase",
            Self::FillEmpty(_) => "Fill Empty Cells",
        }
    }

    /// Returns None for an unchanged value. Errors use one-based data positions.
    pub fn clean_value(
        &self,
        value: &[u8],
        data_row: u64,
        column: usize,
        limit: usize,
    ) -> Result<Option<Vec<u8>>, QuarryError> {
        if let Self::FillEmpty(fill) = self {
            if !value.is_empty() || fill.is_empty() {
                return Ok(None);
            }
            if fill.len() > limit {
                return Err(QuarryError::RecordTooLarge { limit });
            }
            return Ok(Some(fill.clone()));
        }
        let text = std::str::from_utf8(value)
            .map_err(|_| QuarryError::InvalidCleanupText { data_row, column })?;
        if matches!(self, Self::Trim) {
            let trimmed = text.trim();
            return Ok((trimmed.len() != value.len()).then(|| trimmed.as_bytes().to_vec()));
        }
        // str::to_lowercase includes contextual mappings such as Greek final sigma.
        // Unicode case mapping expands at most three scalars per input scalar;
        // this temporary remains bounded by the configured input-record limit.
        let result = match self {
            Self::Uppercase => text.to_uppercase().into_bytes(),
            Self::Lowercase => text.to_lowercase().into_bytes(),
            _ => unreachable!(),
        };
        if result.len() > limit {
            return Err(QuarryError::RecordTooLarge { limit });
        }
        Ok((result != value).then_some(result))
    }
}

/// Column indices must be sorted, unique, zero-based physical file columns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnCleanup {
    pub columns: Vec<usize>,
    pub operation: CleanupOperation,
}

impl ColumnCleanup {
    pub fn validate(&self) -> Result<(), QuarryError> {
        if self.columns.is_empty()
            || self.columns.len() > MAX_TRANSFORMATION_COLUMNS
            || self
                .columns
                .iter()
                .any(|c| *c >= MAX_TRANSFORMATION_COLUMNS)
            || self.columns.windows(2).any(|w| w[0] >= w[1])
        {
            return Err(QuarryError::InvalidOption(
                "cleanup requires sorted, unique, supported file columns",
            ));
        }
        if let CleanupOperation::FillEmpty(value) = &self.operation {
            std::str::from_utf8(value)
                .map_err(|_| QuarryError::InvalidOption("fill value must be valid UTF-8"))?;
        }
        Ok(())
    }

    pub fn apply_to_fields(
        &self,
        fields: &mut [Vec<u8>],
        data_row: u64,
        limit: usize,
    ) -> Result<u64, QuarryError> {
        self.validate()?;
        self.apply_to_fields_validated(fields, data_row, limit)
    }

    pub(crate) fn apply_to_fields_validated(
        &self,
        fields: &mut [Vec<u8>],
        data_row: u64,
        limit: usize,
    ) -> Result<u64, QuarryError> {
        let mut bytes = fields.iter().fold(0usize, |n, f| n.saturating_add(f.len()));
        let mut changed = 0;
        for &column in &self.columns {
            // Missing fields stay missing. Cleanup never pads a ragged row.
            let Some(value) = fields.get_mut(column) else {
                continue;
            };
            if let Some(next) = self
                .operation
                .clean_value(value, data_row, column + 1, limit)?
            {
                bytes = bytes.saturating_sub(value.len()).saturating_add(next.len());
                if bytes > limit {
                    return Err(QuarryError::RecordTooLarge { limit });
                }
                *value = next;
                changed += 1;
            }
        }
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_cleanup_preserves_internal_text_and_reports_actual_changes() {
        let trim = CleanupOperation::Trim;
        assert_eq!(
            trim.clean_value(" \tAlice  Smith\u{a0}\r\n".as_bytes(), 1, 1, 100)
                .unwrap(),
            Some(b"Alice  Smith".to_vec())
        );
        assert_eq!(trim.clean_value(b"Alice  Smith", 1, 1, 100).unwrap(), None);
        assert_eq!(
            CleanupOperation::Uppercase
                .clean_value("Straße".as_bytes(), 1, 1, 100)
                .unwrap(),
            Some(b"STRASSE".to_vec())
        );
        assert_eq!(
            CleanupOperation::Lowercase
                .clean_value("İ".as_bytes(), 1, 1, 100)
                .unwrap(),
            Some("i\u{307}".as_bytes().to_vec())
        );
        assert_eq!(
            CleanupOperation::Lowercase
                .clean_value("ΟΣ".as_bytes(), 1, 1, 100)
                .unwrap(),
            Some("ος".as_bytes().to_vec())
        );
    }

    #[test]
    fn fill_distinguishes_empty_whitespace_missing_and_invalid_nonempty_values() {
        let cleanup = ColumnCleanup {
            columns: vec![0, 1, 2, 3],
            operation: CleanupOperation::FillEmpty(b"new".to_vec()),
        };
        let mut fields = vec![vec![], b"  ".to_vec(), vec![0xff]];
        assert_eq!(cleanup.apply_to_fields(&mut fields, 1, 100).unwrap(), 1);
        assert_eq!(fields, vec![b"new".to_vec(), b"  ".to_vec(), vec![0xff]]);
    }

    #[test]
    fn text_errors_and_expansion_are_explicit_and_bounded() {
        assert!(matches!(
            CleanupOperation::Trim.clean_value(&[0xff], 7, 3, 100),
            Err(QuarryError::InvalidCleanupText {
                data_row: 7,
                column: 3
            })
        ));
        assert!(matches!(
            CleanupOperation::Uppercase.clean_value("ΐ".as_bytes(), 1, 1, 3),
            Err(QuarryError::RecordTooLarge { limit: 3 })
        ));
        let invalid = ColumnCleanup {
            columns: vec![1, 1],
            operation: CleanupOperation::Trim,
        };
        assert!(invalid.validate().is_err());
    }
}
