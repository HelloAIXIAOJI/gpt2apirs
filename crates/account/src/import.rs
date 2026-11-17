//! 账号导入：把各种粘贴文本解析成 [`NewAccount`]。

use serde_json::{Map, Value};

use crate::model::{Plan, Source};
use crate::repo::NewAccount;

/// 解析错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ImportParseError {
    /// 文本为空。
    #[error("导入内容为空")]
    Empty,

    /// JSON 语法错误。
    #[error("JSON 解析失败：{0}")]
    Json(String),

    /// 解析成功但没找到任何账号。
    #[error("没有解析出任何账号（缺少 access_token 字段？）")]
    NoAccounts,
}

/// 把任意文本解析成待导入账号。
///
/// 以 `{` 或 `[` 开头视为 JSON，否则按「一行一个 token」处理。
pub fn parse_any(text: &str) -> Result<Vec<NewAccount>, ImportParseError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(ImportParseError::Empty);
    }
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        parse_json(trimmed)
    } else {
        let tokens = parse_token_lines(trimmed);
        if tokens.is_empty() {
            return Err(ImportParseError::NoAccounts);
        }
        Ok(tokens
            .into_iter()
            .map(|access_token| NewAccount {
                access_token,
                plan: Plan::Unknown,
                source: Source::Web,
                ..Default::default()
            })
            .collect())
    }
}

/// 解析「一行一个 token」，去重去空。
pub fn parse_token_lines(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter(|line| !line.starts_with('#'))
        .filter(|line| seen.insert((*line).to_string()))
        .map(str::to_string)
        .collect()
}

/// 解析 JSON（数组 / 单对象 / 任意包装层）。
pub fn parse_json(text: &str) -> Result<Vec<NewAccount>, ImportParseError> {
    let value: Value =
        serde_json::from_str(text).map_err(|error| ImportParseError::Json(error.to_string()))?;
    let mut collected = Vec::new();
    collect(&value, &mut collected);

    // 按 token 去重（保持首次出现顺序）
    let mut seen = std::collections::HashSet::new();
    collected.retain(|account| seen.insert(account.access_token.clone()));

    if collected.is_empty() {
        return Err(ImportParseError::NoAccounts);
    }
    Ok(collected)
}

fn collect(value: &Value, out: &mut Vec<NewAccount>) {
    collect_with(value, out, None);
}

/// 递归收集。外层对象作为上下文传下去：token 藏在子对象里时，
/// 父层的邮箱与套餐不能丢（凭证嵌套的导出格式很常见）。
fn collect_with(value: &Value, out: &mut Vec<NewAccount>, context: Option<&Map<String, Value>>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_with(item, out, context);
            }
        }
        Value::Object(map) => {
            if let Some(access_token) = pick_string(map, &["access_token", "accessToken", "token"]) {
                let merged = merge(context, map);
                out.push(from_object(&merged, access_token));
                return;
            }
            // 没有 token 字段：继续往子节点找（兼容 {data:[...]}、{accounts:[...]}、{credentials:{...}}）
            for child in map.values() {
                collect_with(child, out, Some(map));
            }
        }
        Value::String(text) => {
            // 字符串数组里的裸 token：部分导出就是 ["eyJ...", "eyJ..."]
            if looks_like_jwt(text) {
                let empty = Map::new();
                out.push(from_object(context.unwrap_or(&empty), text.trim().to_string()));
            }
        }
        _ => {}
    }
}

/// 合并父层与当前层，当前层优先。
fn merge(context: Option<&Map<String, Value>>, current: &Map<String, Value>) -> Map<String, Value> {
    let mut merged = context.cloned().unwrap_or_default();
    for (key, value) in current {
        merged.insert(key.clone(), value.clone());
    }
    merged
}

/// 是否像 JWT（三段 base64url 且足够长），用于从字符串数组里挑 token。
pub fn looks_like_jwt(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.len() < 40 {
        return false;
    }
    let parts: Vec<&str> = trimmed.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '=')
        })
}

fn from_object(map: &Map<String, Value>, access_token: String) -> NewAccount {
    // 套餐：`plan_type` 优先；`type` 若是 codex 则视为导出格式而非套餐
    let raw_type = pick_string(map, &["type"]).unwrap_or_default();
    let plan = pick_string(map, &["plan_type"])
        .map(|value| Plan::parse(&value))
        .or_else(|| {
            if raw_type.eq_ignore_ascii_case("codex") {
                None
            } else if raw_type.is_empty() {
                None
            } else {
                Some(Plan::parse(&raw_type))
            }
        })
        .unwrap_or(Plan::Unknown);

    let source = if raw_type.eq_ignore_ascii_case("codex")
        || pick_string(map, &["export_type"]).is_some_and(|v| v.eq_ignore_ascii_case("codex"))
        || pick_string(map, &["source_type"]).is_some_and(|v| v.eq_ignore_ascii_case("codex"))
    {
        Source::Codex
    } else {
        Source::Web
    };

    // 邮箱可能在顶层，也可能在 `user` / `profile` 子对象里
    let email = pick_string(map, &["email"]).or_else(|| {
        map.get("user")
            .and_then(Value::as_object)
            .and_then(|user| pick_string(user, &["email"]))
    });

    NewAccount {
        access_token,
        refresh_token: pick_string(map, &["refresh_token", "refreshToken"]),
        id_token: pick_string(map, &["id_token", "idToken"]),
        email,
        upstream_account_id: pick_string(map, &["account_id", "accountId"]),
        plan,
        source,
        quota: 0,
        quota_confirmed: false,
        proxy: pick_string(map, &["proxy", "proxy_url", "proxyUrl"])
            .or_else(|| {
                map.get("proxy")
                    .and_then(Value::as_object)
                    .and_then(|proxy| pick_string(proxy, &["url", "server"]))
            })
            .unwrap_or_default(),
    }
}

fn pick_string(map: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(value) = map.get(*key).and_then(Value::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_token_lines_and_dedupes() {
        let tokens = parse_token_lines("a\n b \n\na\n# comment\n");
        assert_eq!(tokens, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn parses_lines_via_parse_any() {
        let accounts = parse_any("tok1\ntok2\n").expect("应解析");
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].access_token, "tok1");
        assert_eq!(accounts[0].plan, Plan::Unknown);
        assert_eq!(accounts[0].source, Source::Web);
    }

    #[test]
    fn empty_input_is_rejected() {
        assert!(matches!(parse_any("   "), Err(ImportParseError::Empty)));
    }

    #[test]
    fn parses_single_session_object() {
        let text = r#"{"accessToken":"tok-a","user":{"email":"a@b.com"}}"#;
        let accounts = parse_json(text).expect("应解析");
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].access_token, "tok-a");
        assert_eq!(accounts[0].email.as_deref(), Some("a@b.com"));
    }

    #[test]
    fn parses_backup_array_with_snake_case() {
        let text = r#"[{"access_token":"tok-a","refresh_token":"rt-a","id_token":"id-a","plan_type":"Plus"}]"#;
        let accounts = parse_json(text).expect("应解析");
        assert_eq!(accounts[0].refresh_token.as_deref(), Some("rt-a"));
        assert_eq!(accounts[0].id_token.as_deref(), Some("id-a"));
        assert_eq!(accounts[0].plan, Plan::Plus);
    }

    #[test]
    fn parses_nested_wrapper() {
        let text = r#"{"data":[{"access_token":"a"},{"access_token":"b"}]}"#;
        let accounts = parse_json(text).expect("应解析");
        assert_eq!(accounts.len(), 2);
    }

    #[test]
    fn codex_type_becomes_source_not_plan() {
        let text = r#"{"type":"codex","access_token":"tok","email":"c@d.com"}"#;
        let accounts = parse_json(text).expect("应解析");
        assert_eq!(accounts[0].source, Source::Codex);
        assert_eq!(accounts[0].plan, Plan::Unknown, "codex 是导出格式，不是套餐");
    }

    #[test]
    fn business_plan_maps_to_team() {
        let text = r#"{"access_token":"tok","plan_type":"business"}"#;
        assert_eq!(parse_json(text).unwrap()[0].plan, Plan::Team);
    }

    #[test]
    fn dedupes_within_json() {
        let text = r#"[{"access_token":"same"},{"access_token":"same"}]"#;
        let accounts = parse_json(text).expect("应解析");
        assert_eq!(accounts.len(), 1);
    }

    #[test]
    fn nested_credentials_keeps_parent_email_and_plan() {
        let text = r#"{"accounts":[{"email":"cpa@example.com","plan_type":"plus","credentials":{"access_token":"eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJhYmMiLCJlbWFpbCI6ImFAYi5jb20ifQ.c2lnbmF0dXJlLXN0cmluZw","refresh_token":"rt-x"}}]}"#;
        let accounts = parse_json(text).expect("应解析");
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].access_token, "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJhYmMiLCJlbWFpbCI6ImFAYi5jb20ifQ.c2lnbmF0dXJlLXN0cmluZw");
        assert_eq!(accounts[0].refresh_token.as_deref(), Some("rt-x"));
        assert_eq!(
            accounts[0].email.as_deref(),
            Some("cpa@example.com"),
            "父层邮箱不能因为 token 在子对象里就丢掉"
        );
        assert_eq!(accounts[0].plan, Plan::Plus);
    }

    #[test]
    fn bare_token_strings_in_array_are_accepted() {
        let text = r#"{"tokens":["eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJhYmMiLCJlbWFpbCI6ImFAYi5jb20ifQ.c2lnbmF0dXJlLXN0cmluZw","eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJhYmMiLCJlbWFpbCI6ImFAYi5jb20ifQ.c2lnbmF0dXJlLXN0cmluZw.extra"]}"#;
        let accounts = parse_json(text).expect("应解析");
        assert_eq!(accounts.len(), 1, "只有三段式 JWT 才算 token");
        assert_eq!(accounts[0].access_token, "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiJhYmMiLCJlbWFpbCI6ImFAYi5jb20ifQ.c2lnbmF0dXJlLXN0cmluZw");
    }

    #[test]
    fn short_strings_are_not_tokens() {
        let text = r#"{"tokens":["free","a@b.com"]}"#;
        assert!(matches!(parse_json(text), Err(ImportParseError::NoAccounts)));
    }

    #[test]
    fn proxy_aliases_and_nested_object() {
        let text = r#"[{"access_token":"tok-a","proxy_url":"http://127.0.0.1:7890"},{"access_token":"tok-b","proxy":{"url":"socks5://127.0.0.1:1080"}}]"#;
        let accounts = parse_json(text).expect("应解析");
        assert_eq!(accounts[0].proxy, "http://127.0.0.1:7890");
        assert_eq!(accounts[1].proxy, "socks5://127.0.0.1:1080");
    }

    #[test]
    fn token_alias_is_accepted() {
        let text = r#"{"accounts":[{"token":"tok-z","email":"z@x.com"}]}"#;
        let accounts = parse_json(text).expect("应解析");
        assert_eq!(accounts[0].access_token, "tok-z");
        assert_eq!(accounts[0].email.as_deref(), Some("z@x.com"));
    }

    #[test]
    fn json_without_tokens_is_rejected() {
        let text = r#"{"hello":"world"}"#;
        assert!(matches!(parse_json(text), Err(ImportParseError::NoAccounts)));
    }

    #[test]
    fn malformed_json_reports_error() {
        assert!(matches!(
            parse_json("{not json"),
            Err(ImportParseError::Json(_))
        ));
    }

    #[test]
    fn picks_proxy_when_present() {
        let text = r#"{"access_token":"tok","proxy":"http://127.0.0.1:7890"}"#;
        assert_eq!(parse_json(text).unwrap()[0].proxy, "http://127.0.0.1:7890");
    }

    #[test]
    fn quota_starts_unconfirmed() {
        let text = r#"[{"access_token":"tok","quota":99}]"#;
        let accounts = parse_json(text).expect("应解析");
        assert_eq!(accounts[0].quota, 0, "导入时不信任何自带配额");
        assert!(!accounts[0].quota_confirmed);
    }
}
