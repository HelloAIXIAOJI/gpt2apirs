//! 列表分页：每页条数来自全局设置，页码窗口统一由这里算。
//!
//! 后台有五个列表（账号、图片、日志、任务、密钥），分页行为必须一致，
//! 所以页码窗口只实现一次，页面只负责把 `PageWindow` 交给模板。

use serde::Serialize;

use gpt2apirs_core::settings::RuntimeSettings;

/// 不分页时的硬上限：设置成 0 也不能把整库拉进内存。
pub const UNLIMITED_CAP: i64 = 1000;

/// 页码窗口里的一格。
#[derive(Debug, Clone, Serialize)]
pub struct PageNumber {
    /// 显示的页码；`None` 表示省略号。
    pub number: Option<i64>,
    /// 链接；省略号没有链接。
    pub href: String,
    /// 显示文本（页码或省略号）。
    pub label: String,
    /// 是否当前页。
    pub current: bool,
    /// 是否省略号。
    pub gap: bool,
}

/// 分页控件的数据。
#[derive(Debug, Clone, Serialize)]
pub struct PageWindow {
    /// 当前页（从 1 开始）。
    pub page: i64,
    /// 总页数（至少 1）。
    pub pages: i64,
    /// 每页条数。
    pub size: i64,
    /// 总条数。
    pub total: i64,
    /// 上一页链接。
    pub prev_href: String,
    /// 下一页链接。
    pub next_href: String,
    /// 是否有上一页。
    pub has_prev: bool,
    /// 是否有下一页。
    pub has_next: bool,
    /// 页码格子（含省略号）。
    pub numbers: Vec<PageNumber>,
    /// 形如「第 1–50 条 / 共 132 条」。
    pub summary: String,
    /// 只有一页时整块隐藏。
    pub visible: bool,
}

/// 从设置里读每页条数：0（或负数）表示不分页，但仍受 [`UNLIMITED_CAP`] 限制。
pub fn page_size(settings: &RuntimeSettings) -> i64 {
    let raw = settings.get_i64("list_page_size");
    if raw <= 0 {
        UNLIMITED_CAP
    } else {
        raw.clamp(1, UNLIMITED_CAP)
    }
}

/// 页码窗口：始终显示首页、末页、当前页左右各一页，其余用省略号。
pub fn numbers(page: i64, pages: i64, base: &str) -> Vec<PageNumber> {
    let mut wanted: Vec<i64> = Vec::new();
    if pages <= 7 {
        // 页数不多时全列，省掉省略号让点击更直接
        for value in 1..=pages {
            wanted.push(value);
        }
        return wanted
            .into_iter()
            .map(|value| PageNumber {
                number: Some(value),
                href: format!("{base}{value}"),
                label: value.to_string(),
                current: value == page,
                gap: false,
            })
            .collect();
    }
    for value in [1, page - 1, page, page + 1, pages] {
        if value >= 1 && value <= pages && !wanted.contains(&value) {
            wanted.push(value);
        }
    }
    wanted.sort_unstable();

    let mut out = Vec::new();
    let mut previous: Option<i64> = None;
    for value in wanted {
        if let Some(last) = previous {
            if value - last > 1 {
                out.push(PageNumber {
                    number: None,
                    href: String::new(),
                    label: "…".to_string(),
                    current: false,
                    gap: true,
                });
            }
        }
        out.push(PageNumber {
            number: Some(value),
            href: format!("{base}{value}"),
            label: value.to_string(),
            current: value == page,
            gap: false,
        });
        previous = Some(value);
    }
    out
}

/// 组装分页控件。`href_prefix` 是页码前缀，例如 `/logs?hours=24&page=`。
pub fn window(total: i64, requested_page: i64, size: i64, href_prefix: &str) -> PageWindow {
    let size = size.clamp(1, UNLIMITED_CAP);
    let pages = ((total.max(0) + size - 1) / size).max(1);
    let page = requested_page.max(1).min(pages);
    let first = (page - 1) * size + 1;
    let last = (page * size).min(total.max(0));
    PageWindow {
        page,
        pages,
        size,
        total,
        prev_href: format!("{href_prefix}{}", (page - 1).max(1)),
        next_href: format!("{href_prefix}{}", (page + 1).min(pages)),
        has_prev: page > 1,
        has_next: page < pages,
        numbers: numbers(page, pages, href_prefix),
        summary: if total <= 0 {
            "暂无数据".to_string()
        } else {
            format!("第 {first}–{last} 条 / 共 {total} 条")
        },
        visible: pages > 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visible_numbers(page: i64, pages: i64) -> Vec<Option<i64>> {
        numbers(page, pages, "/x?page=")
            .into_iter()
            .map(|item| item.number)
            .collect()
    }

    #[test]
    fn single_page_has_no_ellipsis() {
        assert_eq!(visible_numbers(1, 1), vec![Some(1)]);
    }

    #[test]
    fn middle_page_shows_ellipsis_on_both_sides() {
        // 1 … 8 9 10 … 20
        assert_eq!(
            visible_numbers(9, 20),
            vec![Some(1), None, Some(8), Some(9), Some(10), None, Some(20)]
        );
    }

    #[test]
    fn few_pages_are_all_listed() {
        assert_eq!(
            visible_numbers(2, 5),
            vec![Some(1), Some(2), Some(3), Some(4), Some(5)],
            "页数不多时应全部列出"
        );
    }

    #[test]
    fn second_page_has_no_left_ellipsis() {
        // 20 页时第 2 页：1 2 3 … 20
        assert_eq!(
            visible_numbers(2, 20),
            vec![Some(1), Some(2), Some(3), None, Some(20)]
        );
    }

    #[test]
    fn window_clamps_page_and_reports_range() {
        let page = window(132, 99, 50, "/logs?page=");
        assert_eq!(page.pages, 3);
        assert_eq!(page.page, 3);
        assert_eq!(page.summary, "第 101–132 条 / 共 132 条");
        assert!(!page.has_next);
        assert!(page.has_prev);
        assert_eq!(page.next_href, "/logs?page=3");
    }

    #[test]
    fn zero_total_says_no_data() {
        let page = window(0, 1, 50, "/x?page=");
        assert_eq!(page.pages, 1);
        assert_eq!(page.summary, "暂无数据");
        assert!(!page.visible);
    }
}
