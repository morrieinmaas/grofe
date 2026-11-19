//! Reflow of cold history rows.

use super::SnapCell;

/// What a cold row remembers besides its cells.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ColdMeta {
    pub wrapped: bool,
    pub used: usize,
}

// ponytail: identity until T7
#[allow(dead_code)]
pub fn reflow_rows(
    rows: &[(Vec<SnapCell>, ColdMeta)],
    _cols: usize,
) -> Vec<(Vec<SnapCell>, ColdMeta)> {
    rows.to_vec()
}
