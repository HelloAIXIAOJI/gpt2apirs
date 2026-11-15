//! 账号数据模型。
//!
//! 设计见 `docs/account-pool-design.md` 第三、四章。
//!
//! # 两个维度
//!
//! 持久化边界：[`AccountStatus`] 落库；[`crate::credentials::CredentialAvailability`]
//! 每次从 JWT 现算，不落库（token 一换可用性就变，存了必然过期）。
//!
//! **状态在库里存小写英文**，中文只出现在 [`AccountStatus::label`]。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 业务状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccountStatus {
    /// 正常，可被选中。
    Normal,
    /// 配额耗尽（限流）。
    Limited,
    /// 凭证失效。
    Abnormal,
    /// 人工禁用。
    Disabled,
}

impl AccountStatus {
    /// 数据库存储值。
    pub fn as_str(self) -> &'static str {
        match self {
            AccountStatus::Normal => "normal",
            AccountStatus::Limited => "limited",
            AccountStatus::Abnormal => "abnormal",
            AccountStatus::Disabled => "disabled",
        }
    }

    /// 从数据库值解析。未知值返回 `None`——**不允许**默默当成正常号。
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "normal" => Some(AccountStatus::Normal),
            "limited" => Some(AccountStatus::Limited),
            "abnormal" => Some(AccountStatus::Abnormal),
            "disabled" => Some(AccountStatus::Disabled),
            _ => None,
        }
    }

    /// 前端展示用中文。
    pub fn label(self) -> &'static str {
        match self {
            AccountStatus::Normal => "正常",
            AccountStatus::Limited => "限流",
            AccountStatus::Abnormal => "异常",
            AccountStatus::Disabled => "禁用",
        }
    }

    /// 是否可作为图片请求的候选（仅 Normal）。
    pub fn selectable_for_image(self) -> bool {
        matches!(self, AccountStatus::Normal)
    }

    /// 是否可作为文本请求的候选（Normal 或 Limited，文本不消耗图片配额）。
    pub fn selectable_for_text(self) -> bool {
        matches!(self, AccountStatus::Normal | AccountStatus::Limited)
    }
}

/// 套餐类型。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Plan {
    /// 免费。
    Free,
    /// Plus。
    Plus,
    /// Pro。
    Pro,
    /// ProLite。
    ProLite,
    /// Team（含 Business）。
    Team,
    /// Enterprise。
    Enterprise,
    /// 未知（导入时没给）。
    ///
    /// 作为 [`Default`]：导入时若没写套餐，**不能假设是 Free** ——
    /// 假设错了会导致按免费号的限制去调度付费号，或反过来。
    #[default]
    Unknown,
}

impl Plan {
    /// 数据库存储值。
    pub fn as_str(self) -> &'static str {
        match self {
            Plan::Free => "free",
            Plan::Plus => "plus",
            Plan::Pro => "pro",
            Plan::ProLite => "pro_lite",
            Plan::Team => "team",
            Plan::Enterprise => "enterprise",
            Plan::Unknown => "unknown",
        }
    }

    /// 从任意写法解析：上游导出里 `type` / `plan_type` 的大小写与别名都不统一。
    pub fn parse(value: &str) -> Self {
        let raw = value.trim().to_ascii_lowercase();
        let compact: String = raw.chars().filter(|c| *c != '_' && *c != '-' && *c != ' ').collect();
        match compact.as_str() {
            "" => Plan::Unknown,
            "free" => Plan::Free,
            "plus" => Plan::Plus,
            "pro" => Plan::Pro,
            "prolite" | "prolitee" => Plan::ProLite,
            "team" | "business" => Plan::Team,
            "enterprise" => Plan::Enterprise,
            _ => Plan::Unknown,
        }
    }
}

/// 账号来源。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// 网页版。
    ///
    /// 作为 [`Default`]：来源未知时按 web 处理。
    #[default]
    Web,
    /// Codex。
    Codex,
}

impl Source {
    /// 数据库存储值。
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Web => "web",
            Source::Codex => "codex",
        }
    }

    /// 解析。未知值返回 `None`。
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "codex" => Some(Source::Codex),
            "web" => Some(Source::Web),
            _ => None,
        }
    }
}

/// 一个账号。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    /// 内部稳定 ID（不随 token 轮换变化）。
    pub id: Uuid,

    /// access token。
    pub access_token: String,
    /// refresh token。
    pub refresh_token: Option<String>,
    /// id token。
    pub id_token: Option<String>,

    /// 邮箱。
    pub email: Option<String>,
    /// 上游 account_id。
    pub upstream_account_id: Option<String>,

    /// 套餐。
    pub plan: Plan,
    /// 来源。
    pub source: Source,

    /// 业务状态。
    pub status: AccountStatus,
    /// 剩余生图配额。
    pub quota: i32,
    /// 配额是否被远端确认过。
    pub quota_confirmed: bool,
    /// 限流恢复时间。
    pub restore_at: Option<DateTime<Utc>>,

    /// 账号级代理（加入分组时是组地址的副本）。
    pub proxy: String,
    /// 所属代理分组。
    pub proxy_group_id: Option<Uuid>,

    /// 成功次数。
    pub success_count: i64,
    /// 失败次数。
    pub fail_count: i64,
    /// 连续被判失效次数。
    pub invalid_count: i32,

    /// 最近使用时间。
    pub last_used_at: Option<DateTime<Utc>>,
    /// 最近被判失效时间。
    pub last_invalid_at: Option<DateTime<Utc>>,
    /// 最近错误（**已脱敏**）。
    pub last_error: Option<String>,
    /// 最近错误时间。
    pub last_error_at: Option<DateTime<Utc>>,
    /// 最近远端校验时间。
    pub last_checked_at: Option<DateTime<Utc>>,
    /// 最近 token 刷新时间。
    pub last_refresh_at: Option<DateTime<Utc>>,

    /// 创建时间。
    pub created_at: DateTime<Utc>,
    /// 更新时间。
    pub updated_at: DateTime<Utc>,
}

impl Account {
    /// 计算 access token 的 sha256（小写 hex），用于唯一索引与查重。
    pub fn token_sha(access_token: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(access_token.trim().as_bytes());
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// 是否可以作为图片请求候选（业务层面）。
    pub fn selectable_for_image(&self) -> bool {
        self.status.selectable_for_image() && self.quota > 0
    }

    /// 是否可以作为文本请求候选（业务层面）。
    pub fn selectable_for_text(&self) -> bool {
        self.status.selectable_for_text()
    }

    /// 打日志/返回给前端时用的短标识。
    pub fn label(&self) -> String {
        self.email
            .clone()
            .filter(|email| !email.trim().is_empty())
            .unwrap_or_else(|| self.id.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_roundtrip() {
        for status in [
            AccountStatus::Normal,
            AccountStatus::Limited,
            AccountStatus::Abnormal,
            AccountStatus::Disabled,
        ] {
            assert_eq!(AccountStatus::parse(status.as_str()), Some(status));
        }
    }


    #[test]
    fn status_unknown_is_rejected() {
        assert_eq!(AccountStatus::parse("nonsense"), None);
        assert_eq!(AccountStatus::parse("正常"), None, "中文写法是旧数据，不再接受");
        assert_eq!(AccountStatus::parse(""), None);
    }

    #[test]
    fn status_labels_are_chinese() {
        assert_eq!(AccountStatus::Normal.label(), "正常");
        assert_eq!(AccountStatus::Limited.label(), "限流");
        assert_eq!(AccountStatus::Abnormal.label(), "异常");
        assert_eq!(AccountStatus::Disabled.label(), "禁用");
    }

    #[test]
    fn selectable_rules() {
        assert!(AccountStatus::Normal.selectable_for_image());
        assert!(!AccountStatus::Limited.selectable_for_image());
        assert!(!AccountStatus::Abnormal.selectable_for_image());
        assert!(!AccountStatus::Disabled.selectable_for_image());

        // 文本允许限流号
        assert!(AccountStatus::Normal.selectable_for_text());
        assert!(AccountStatus::Limited.selectable_for_text());
        assert!(!AccountStatus::Abnormal.selectable_for_text());
    }

    #[test]
    fn plan_aliases() {
        assert_eq!(Plan::parse("plus"), Plan::Plus);
        assert_eq!(Plan::parse("PLUS"), Plan::Plus);
        assert_eq!(Plan::parse("business"), Plan::Team);
        assert_eq!(Plan::parse("pro-lite"), Plan::ProLite);
        assert_eq!(Plan::parse("prolite"), Plan::ProLite);
        assert_eq!(Plan::parse("Pro Lite"), Plan::ProLite);
        assert_eq!(Plan::parse("enterprise"), Plan::Enterprise);
        assert_eq!(Plan::parse("free"), Plan::Free);
        assert_eq!(Plan::parse(""), Plan::Unknown);
        assert_eq!(Plan::parse("weird"), Plan::Unknown);
    }

    #[test]
    fn plan_roundtrip() {
        for plan in [Plan::Free, Plan::Plus, Plan::Pro, Plan::ProLite, Plan::Team, Plan::Enterprise, Plan::Unknown] {
            assert_eq!(Plan::parse(plan.as_str()), plan, "{:?}", plan);
        }
    }

    #[test]
    fn source_parse() {
        assert_eq!(Source::parse("codex"), Some(Source::Codex));
        assert_eq!(Source::parse("web"), Some(Source::Web));
        assert_eq!(Source::parse(""), None);
    }

    #[test]
    fn token_sha_is_stable_and_trims() {
        let a = Account::token_sha("  abc123  ");
        let b = Account::token_sha("abc123");
        assert_eq!(a, b, "应 trim 后再哈希");
        assert_eq!(a.len(), 64, "sha256 hex 应为 64 字符");
        assert_ne!(a, Account::token_sha("abc124"));
    }
}
