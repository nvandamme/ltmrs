//! One pagination contract for the retrieval surface (Wave A3a).
//!
//! Both `memory_read` and `semantic_search` page over a MATCHING set that may
//! be larger than any served window. Callers must never infer the set size
//! from an already-truncated vector: every paged response carries the full
//! `total` plus `has_more`/`next_offset`, where `total` is exact when the
//! ranking pool covered the whole matching set and an explicit lower bound
//! (with `has_more` forced) when a candidate leg hit its fetch cap.

use ltmrs_domain::command::{DomainError, DomainErrorCode, DomainResult};

/// Application-level maximum pagination window: bounds `offset + limit`
/// everywhere pagination arithmetic occurs, so engine fetch windows and MMR
/// selection stay finite. Per-tool page caps (memory_read 100, semantic 30)
/// apply below this window.
pub const MAX_PAGINATION_WINDOW: usize = 1000;

/// One served page over a larger matching set. Wire shapes are unchanged:
/// tools render exactly the same JSON keys from these values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    /// The served page items, in rank order.
    pub items: Vec<T>,
    /// Full matching-set size (exact), or a lower bound when
    /// `pool_truncated` forced `has_more`.
    pub total: usize,
    /// The validated offset this page starts at.
    pub offset: usize,
    /// The validated per-page limit.
    pub limit: usize,
    /// True when further pages may exist.
    pub has_more: bool,
    /// Continuation offset when `has_more`, else `None`.
    pub next_offset: Option<usize>,
}

/// Validate raw pagination input: checked `offset + limit` (overflow is a
/// typed error, never a wrap) bounded by [`MAX_PAGINATION_WINDOW`].
/// Returns the exclusive window end for fetch sizing.
pub fn validate_window(offset: usize, limit: usize) -> DomainResult<usize> {
    let end = offset.checked_add(limit).ok_or_else(|| {
        DomainError::new(
            DomainErrorCode::Validation,
            format!("pagination offset {offset} + limit {limit} overflows"),
        )
    })?;
    if end > MAX_PAGINATION_WINDOW {
        return Err(DomainError::new(
            DomainErrorCode::Validation,
            format!(
                "pagination window {end} (offset {offset} + limit {limit}) exceeds maximum {MAX_PAGINATION_WINDOW}"
            ),
        ));
    }
    Ok(end)
}

/// `has_more`/`next_offset` from an exact-or-bounded total: a truncated pool
/// forces continuation even when the served window covered the pool, so a
/// lower-bound total never presents as complete.
pub fn page_flags(total: usize, window_end: usize, pool_truncated: bool) -> (bool, Option<usize>) {
    if total == 0 {
        return (false, None);
    }
    let more = window_end < total || pool_truncated;
    (more, more.then_some(window_end))
}

/// Slice a page out of a rank-ordered vector with an independently known
/// total: `items` is the full ordered set (snapshot scans) or the fetched
/// window starting at rank 0 (engine results, sized to cover `window_end`).
/// The total is never derived from `items.len()`.
pub fn paginate_vec<T>(
    items: Vec<T>,
    total: usize,
    offset: usize,
    limit: usize,
    pool_truncated: bool,
) -> DomainResult<Page<T>> {
    let window_end = validate_window(offset, limit)?;
    let page: Vec<T> = items.into_iter().skip(offset).take(limit).collect();
    let (has_more, next_offset) = page_flags(total, window_end, pool_truncated);
    Ok(Page {
        items: page,
        total,
        offset,
        limit,
        has_more,
        next_offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_end_is_offset_plus_limit() {
        assert_eq!(validate_window(0, 30).unwrap(), 30);
        assert_eq!(validate_window(499, 30).unwrap(), 529);
        assert_eq!(validate_window(970, 30).unwrap(), 1000);
    }

    #[test]
    fn window_overflow_is_validation_error() {
        let err = validate_window(usize::MAX, 30).unwrap_err();
        assert_eq!(err.code, DomainErrorCode::Validation);
        let err = validate_window(0, usize::MAX).unwrap_err();
        assert_eq!(err.code, DomainErrorCode::Validation);
    }

    #[test]
    fn window_beyond_max_is_validation_error() {
        let err = validate_window(971, 30).unwrap_err();
        assert_eq!(err.code, DomainErrorCode::Validation);
        assert!(err.message.contains("1000"), "got: {}", err.message);
    }

    #[test]
    fn flags_exact_totals() {
        assert_eq!(page_flags(500, 30, false), (true, Some(30)));
        assert_eq!(page_flags(500, 500, false), (false, None));
        assert_eq!(page_flags(31, 31, false), (false, None));
        assert_eq!(page_flags(0, 10, false), (false, None));
    }

    #[test]
    fn flags_truncated_pool_forces_continuation() {
        assert_eq!(page_flags(31, 31, true), (true, Some(31)));
    }

    #[test]
    fn paginate_slices_without_recounting() {
        let items: Vec<u64> = (0..500).collect();
        let page = paginate_vec(items, 500, 460, 30, false).unwrap();
        assert_eq!(page.items.len(), 30);
        assert_eq!(page.items[0], 460);
        assert_eq!(page.total, 500);
        assert!(page.has_more);
        assert_eq!(page.next_offset, Some(490));
        let last = paginate_vec((0..500u64).collect(), 500, 500, 30, false).unwrap();
        assert!(last.items.is_empty());
        assert!(!last.has_more);
        assert_eq!(last.next_offset, None);
    }
}
