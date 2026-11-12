//! 代理分组。
//!
//! 一组共享一个代理串。加入分组时组地址会复制进 `accounts.proxy`，
//! 组地址变更时同步重写组内账号 —— 运行时不做 join，账号查询路径保持不变。

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::repo::AccountError;

/// 一个代理分组。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize)]
pub struct ProxyGroup {
    /// 主键。
    pub id: Uuid,
    /// 名称（唯一）。
    pub name: String,
    /// 代理串；空串表示直连。
    pub url: String,
    /// 创建时间。
    pub created_at: DateTime<Utc>,
    /// 更新时间。
    pub updated_at: DateTime<Utc>,
}

/// 带成员数的分组。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProxyGroupView {
    /// 分组本体。
    #[serde(flatten)]
    pub group: ProxyGroup,
    /// 组内账号数。
    pub members: i64,
}

/// 分组仓储。
#[derive(Debug, Clone)]
pub struct ProxyGroupRepo {
    pool: PgPool,
}

impl ProxyGroupRepo {
    /// 构造。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 全部分组，附带成员数。
    pub async fn list(&self) -> Result<Vec<ProxyGroupView>, AccountError> {
        let rows: Vec<ProxyGroup> = sqlx::query_as(SQL_LIST)
            .fetch_all(&self.pool)
            .await?;
        let mut views = Vec::with_capacity(rows.len());
        for group in rows {
            let (members,): (i64,) =
                sqlx::query_as("select count(*) from accounts where proxy_group_id = $1")
                    .bind(group.id)
                    .fetch_one(&self.pool)
                    .await?;
            views.push(ProxyGroupView { group, members });
        }
        Ok(views)
    }

    /// 取一个。
    pub async fn get(&self, id: Uuid) -> Result<Option<ProxyGroup>, AccountError> {
        let row = sqlx::query_as::<_, ProxyGroup>("select * from proxy_groups where id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    /// 新建。名称重复会返回唯一约束错误。
    pub async fn create(&self, name: &str, url: &str) -> Result<ProxyGroup, AccountError> {
        let row = sqlx::query_as::<_, ProxyGroup>(SQL_CREATE)
            .bind(Uuid::new_v4())
            .bind(name.trim())
            .bind(url.trim())
            .fetch_one(&self.pool)
            .await?;
        Ok(row)
    }

    /// 改名称和地址。
    pub async fn update(&self, id: Uuid, name: &str, url: &str) -> Result<ProxyGroup, AccountError> {
        let row = sqlx::query_as::<_, ProxyGroup>(SQL_UPDATE)
            .bind(id)
            .bind(name.trim())
            .bind(url.trim())
            .fetch_one(&self.pool)
            .await?;
        Ok(row)
    }

    /// 删除。返回是否命中。
    pub async fn delete(&self, id: Uuid) -> Result<bool, AccountError> {
        let result = sqlx::query("delete from proxy_groups where id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

const SQL_LIST: &str = "select * from proxy_groups order by created_at";

const SQL_CREATE: &str = r#"insert into proxy_groups (id, name, url) values ($1, $2, $3)
  returning *"#;

const SQL_UPDATE: &str = r#"update proxy_groups set name = $2, url = $3, updated_at = now()
  where id = $1 returning *"#;
