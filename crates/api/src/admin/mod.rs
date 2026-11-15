//! 管理后台的服务端渲染入口。
//!
//! 页面全部由 Askama 在服务端渲染，浏览器只跑 htmx 做局部替换；
//! 这里没有前端构建，资源用 `include_str!` 打进二进制。

#[cfg(test)]
use askama::Template;

/// 提供给浏览器的 htmx 脚本。
pub const HTMX_JS: &str = include_str!("../../assets/htmx.min.js");

/// 版式骨架；各页面模板通过 `{% extends "base.html" %}` 复用。
#[derive(Template)]
#[template(path = "base.html")]
#[cfg(test)]
struct Skeleton;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_embeds_htmx_and_has_no_local_assets() {
        let html = Skeleton.render().expect("渲染版式");
        assert!(html.contains("/admin/assets/htmx.min.js"), "版式应引用 htmx：{html}");
        assert!(!html.contains("<link rel=\"stylesheet\""), "当前不引入样式文件：{html}");
    }

    #[test]
    fn htmx_asset_is_embedded() {
        assert!(HTMX_JS.len() > 30_000, "htmx 脚本应已打进二进制，实际 {} 字节", HTMX_JS.len());
        assert!(HTMX_JS.contains("htmx"), "内容不像 htmx");
    }
}
