//! 侧边栏导航项。
//!
//! 导航表只有这一份；`ready = false` 的条目渲染成不可点的灰项，
//! 避免出现「点了 404」的死链。

/// 一个导航项。
#[derive(Debug, Clone)]
pub struct NavItem {
    /// 显示名。
    pub label: &'static str,
    /// 目标地址；未建页面也写上，方便以后接。
    pub href: &'static str,
    /// 是否当前页。
    pub active: bool,
    /// 页面是否已经存在。
    pub ready: bool,
}

/// 全部导航项。
const ITEMS: &[(&str, &str, bool)] = &[
    ("概览", "/overview", true),
    ("账号", "/accounts", true),
    ("账号桶", "/buckets", true),
    ("代理", "/proxy", true),
    ("图片", "/gallery", true),
    ("任务", "/tasks", true),
    ("日志", "/logs", true),
    ("会话", "/conversations", true),
    ("记忆", "/memories", true),
    ("密钥", "/keys", true),
    ("备份", "/backups", true),
    ("设置", "/settings", true),
];

/// 按当前路径生成导航。
pub fn items(current: &str) -> Vec<NavItem> {
    ITEMS
        .iter()
        .map(|(label, href, ready)| NavItem {
            label,
            href,
            active: *href == current,
            ready: *ready,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_current_page_active() {
        let items = items("/overview");
        let overview = items.iter().find(|item| item.label == "概览").expect("概览");
        assert!(overview.active, "概览应为当前页");
        assert!(overview.ready, "概览已存在");
        assert_eq!(items.iter().filter(|item| item.active).count(), 1, "只能有一个当前页");
    }

    #[test]
    fn unknown_page_has_no_active_item() {
        assert!(items("/nope").iter().all(|item| !item.active));
    }

    #[test]
    fn ready_pages_are_marked() {
        let items = items("/overview");
        let ready: Vec<&str> = items.iter().filter(|i| i.ready).map(|i| i.label).collect();
        assert_eq!(
            ready,
            vec!["概览", "账号", "账号桶", "代理", "图片", "任务", "日志", "会话", "记忆", "密钥", "备份", "设置"]
        );
    }
}
