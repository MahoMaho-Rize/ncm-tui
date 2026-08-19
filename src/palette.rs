//! Fuzzy command palette. The TUI supplies items; this module only filters.

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaletteItem {
    pub title: String,
    pub hint: String,
    pub keywords: String,
}

pub fn filter_items(items: &[PaletteItem], query: &str) -> Vec<usize> {
    let query = query.trim();
    items
        .iter()
        .enumerate()
        .filter(|(_, item)| fuzzy_match(&item.haystack(), query))
        .map(|(index, _)| index)
        .collect()
}

impl PaletteItem {
    pub fn new(
        title: impl Into<String>,
        hint: impl Into<String>,
        keywords: impl Into<String>,
    ) -> Self {
        Self {
            title: title.into(),
            hint: hint.into(),
            keywords: keywords.into(),
        }
    }

    fn haystack(&self) -> String {
        format!("{} {} {}", self.title, self.hint, self.keywords)
    }
}

pub fn fuzzy_match(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let haystack = haystack.to_lowercase();
    let mut rest = haystack.as_str();
    for character in needle.to_lowercase().chars() {
        match rest.find(character) {
            Some(index) => {
                rest = &rest[index + character.len_utf8()..];
            }
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<PaletteItem> {
        vec![
            PaletteItem::new("本地音乐", "跳转", "local library"),
            PaletteItem::new("导入本地音乐", "I", "import scan"),
            PaletteItem::new("每日推荐", "跳转", "daily"),
        ]
    }

    #[test]
    fn empty_query_keeps_all_items_in_order() {
        assert_eq!(filter_items(&items(), " "), vec![0, 1, 2]);
    }

    #[test]
    fn subsequence_matches_title_or_keywords() {
        let items = items();
        assert_eq!(filter_items(&items, "导入"), vec![1]);
        assert_eq!(filter_items(&items, "lcl"), vec![0]);
        assert_eq!(filter_items(&items, "daily"), vec![2]);
        assert!(filter_items(&items, "zzzz").is_empty());
    }
}
