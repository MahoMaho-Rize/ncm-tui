//! Page windows for online lists. Columns own this state; the UI thread never waits on I/O.

use std::collections::HashSet;

pub const PAGE_SIZE: usize = 50;
pub const PREFETCH_REMAINING: usize = 8;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PaginationInfo {
    pub offset: usize,
    pub limit: usize,
    pub has_more: bool,
    pub total: u64,
    pub loading: bool,
}

impl PaginationInfo {
    pub fn from_fetch(offset: usize, limit: usize, received: usize, total: u64) -> Self {
        let next = offset.saturating_add(received);
        let reached_end = received < limit || (total > 0 && next as u64 >= total);
        Self {
            offset: next,
            limit,
            has_more: !reached_end,
            total,
            loading: false,
        }
    }

    pub fn should_prefetch(self, selected: usize, loaded: usize) -> bool {
        self.has_more
            && !self.loading
            && loaded > 0
            && selected.saturating_add(PREFETCH_REMAINING) + 1 >= loaded
    }

    pub fn display_total(self, loaded: usize) -> (usize, bool) {
        let total = (self.total as usize).max(loaded);
        (total, self.has_more && (self.total == 0 || total > loaded))
    }
}

pub fn merge_unique_by_id<T>(existing: &mut Vec<T>, extra: Vec<T>, id_of: impl Fn(&T) -> u64) {
    let mut seen = existing.iter().map(&id_of).collect::<HashSet<_>>();
    for item in extra {
        let id = id_of(&item);
        if seen.insert(id) {
            existing.push(item);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_short_page_is_terminal_even_when_total_is_unknown() {
        let page = PaginationInfo::from_fetch(0, 50, 12, 0);
        assert!(!page.has_more);
        assert_eq!(page.offset, 12);
    }

    #[test]
    fn full_page_with_known_total_keeps_prefetching() {
        let page = PaginationInfo::from_fetch(0, 50, 50, 80);
        assert!(page.has_more);
        assert_eq!(page.offset, 50);
        assert!(page.should_prefetch(42, 50));
        assert!(!page.should_prefetch(10, 50));
    }

    #[test]
    fn merge_keeps_existing_order_and_skips_duplicates() {
        let mut items = vec![3, 1];
        merge_unique_by_id(&mut items, vec![1, 4, 3, 5], |value| *value as u64);
        assert_eq!(items, vec![3, 1, 4, 5]);
    }
}
