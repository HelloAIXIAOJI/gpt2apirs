//! 账号导出（CPA / Codex 兼容格式）。
//!
//!
//! **导出的前提是 access_token / refresh_token / id_token 三者齐全**，
//! 缺一个就跳过 —— 缺件的导出文件在别的工具里导入会失败，
//! 与其产出半残文件，不如不产出并向调用方说明跳过了多少。

use serde::Serialize;

use crate::credentials;
use crate::model::Account;

/// 一条导出记录。
#[derive(Debug, Clone, Serialize)]
pub struct ExportItem {
    /// 导出格式类型，固定 `codex`。
    #[serde(rename = "type")]
    pub kind: String,
    /// 邮箱。
    pub email: String,
    /// 上游 account_id。
    pub account_id: String,
    /// access token。
    pub access_token: String,
    /// refresh token。
    pub refresh_token: String,
    /// id token。
    pub id_token: String,
    /// access token 到期时间（RFC3339）。
    pub expired: Option<String>,
    /// access token 签发时间（RFC3339）。
    pub last_refresh: Option<String>,
}

/// 导出结果。
#[derive(Debug, Clone, Serialize)]
pub struct ExportReport {
    /// 成功导出的条目。
    pub items: Vec<ExportItem>,
    /// 因缺件被跳过的账号标识。
    pub skipped: Vec<String>,
}

/// 构造导出条目。
pub fn build_export(accounts: &[Account]) -> ExportReport {
    let mut items = Vec::new();
    let mut skipped = Vec::new();

    for account in accounts {
        let access_token = account.access_token.trim();
        let refresh_token = account.refresh_token.as_deref().unwrap_or("").trim();
        let id_token = account.id_token.as_deref().unwrap_or("").trim();
        if access_token.is_empty() || refresh_token.is_empty() || id_token.is_empty() {
            skipped.push(account.label());
            continue;
        }

        items.push(ExportItem {
            kind: "codex".to_string(),
            email: account
                .email
                .clone()
                .or_else(|| credentials::token_email(access_token))
                .unwrap_or_default(),
            account_id: account
                .upstream_account_id
                .clone()
                .or_else(|| credentials::token_upstream_account_id(access_token))
                .unwrap_or_default(),
            access_token: access_token.to_string(),
            refresh_token: refresh_token.to_string(),
            id_token: id_token.to_string(),
            expired: credentials::token_exp(access_token).map(|dt| dt.to_rfc3339()),
            last_refresh: credentials::token_iat(access_token).map(|dt| dt.to_rfc3339()),
        });
    }

    ExportReport { items, skipped }
}

/// 产出「一行一个 access token」的文本。
///
/// 同样跳过缺 refresh/id token 的账号 —— TXT 是给「简单分发」用的，
/// 但既然与 JSON 导出保持同一口径，就都要求三件套齐全。
pub fn build_token_txt(accounts: &[Account]) -> String {
    let report = build_export(accounts);
    report
        .items
        .iter()
        .map(|item| item.access_token.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AccountStatus, Plan, Source};
    use base64::Engine as _;
    use chrono::{Duration, Utc};
    use uuid::Uuid;

    fn jwt(exp_offset_secs: i64) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
        let claims = format!(
            r#"{{"exp":{},"iat":{},"https://api.openai.com/profile":{{"email":"jwt@x.com"}}}}"#,
            (Utc::now() + Duration::seconds(exp_offset_secs)).timestamp(),
            Utc::now().timestamp()
        );
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.as_bytes());
        format!("{header}.{payload}.sig")
    }

    fn account(with_refresh: bool, with_id: bool) -> Account {
        let now = Utc::now();
        Account {
            id: Uuid::new_v4(),
            bucket_id: Uuid::nil(),
            access_token: jwt(3600),
            refresh_token: with_refresh.then(|| "rt".to_string()),
            id_token: with_id.then(|| "id".to_string()),
            email: None,
            upstream_account_id: None,
            plan: Plan::Plus,
            source: Source::Web,
            status: AccountStatus::Normal,
            quota: 1,
            quota_confirmed: true,
        quota_unlimited: false,
            restore_at: None,
            success_count: 0,
            fail_count: 0,
            invalid_count: 0,
            last_used_at: None,
            last_invalid_at: None,
            last_error: None,
            last_error_at: None,
            last_checked_at: None,
            last_refresh_at: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn exports_complete_account() {
        let report = build_export(&[account(true, true)]);
        assert_eq!(report.items.len(), 1);
        assert!(report.skipped.is_empty());
        let item = &report.items[0];
        assert_eq!(item.kind, "codex");
        assert_eq!(item.email, "jwt@x.com", "邮箱应能从 JWT 补出");
        assert!(item.expired.is_some());
        assert!(item.last_refresh.is_some());
    }

    #[test]
    fn skips_account_missing_refresh_token() {
        let report = build_export(&[account(false, true)]);
        assert!(report.items.is_empty());
        assert_eq!(report.skipped.len(), 1);
    }

    #[test]
    fn skips_account_missing_id_token() {
        let report = build_export(&[account(true, false)]);
        assert!(report.items.is_empty());
        assert_eq!(report.skipped.len(), 1);
    }

    #[test]
    fn txt_contains_only_access_tokens() {
        let text = build_token_txt(&[account(true, true), account(true, true)]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            assert!(line.split('.').count() == 3, "每行应是一个 JWT: {line}");
        }
    }

    #[test]
    fn txt_skips_incomplete_accounts() {
        let text = build_token_txt(&[account(true, true), account(false, false)]);
        assert_eq!(text.lines().count(), 1);
    }
}
