//! 异步生图任务：提交后立即返回任务 id，后台排队生成，客户端轮询取结果。
//!
//! 与同步接口共用 image_flow 的执行层，只是把「一直等」换成「任务存储 + 有界并发」。
//! 任务只存内存：进程重启即丢，暂不持久化；运行中删除只摘掉记录，不打断上游请求。

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::state::AppState;

/// 任务里保留的图片元信息（不缓存字节，读时按 rel_path 拼 URL 或读盘）。
#[derive(Debug, Clone)]
pub struct TaskImage {
    /// 相对 DATA_DIR 的路径。
    pub rel_path: String,
    /// MIME。
    pub mime: &'static str,
    /// 宽。
    pub width: u32,
    /// 高。
    pub height: u32,
}

/// 任务状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// 已入队、尚未开始。
    Queued,
    /// 正在生成。
    Running,
    /// 已完成，有图。
    Succeeded,
    /// 已失败。
    Failed,
}

impl TaskStatus {
    /// 对外字符串。
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Queued => "queued",
            TaskStatus::Running => "running",
            TaskStatus::Succeeded => "succeeded",
            TaskStatus::Failed => "failed",
        }
    }

    /// 是否终态。
    pub fn is_terminal(&self) -> bool {
        matches!(self, TaskStatus::Succeeded | TaskStatus::Failed)
    }
}

/// 一个异步生图任务。
#[derive(Debug, Clone)]
pub struct ImageTask {
    /// 任务 id。
    pub id: Uuid,
    /// 状态。
    pub status: TaskStatus,
    /// 提示词。
    pub prompt: String,
    /// 对外模型名。
    pub model: String,
    /// 请求张数。
    pub n: u32,
    /// 返回格式：url / b64_json。
    pub response_format: String,
    /// 输出格式（png/jpeg/webp/gif）；空表示按上游默认。
    pub output_format: Option<String>,
    /// GIF 帧数。
    pub frame_count: Option<u32>,
    /// GIF 每帧时长（毫秒）。
    pub frame_duration_ms: Option<u32>,
    /// 对外基础地址（拼结果 URL 用）。
    pub base_url: String,
    /// 创建时间。
    pub created_at: DateTime<Utc>,
    /// 最近更新时间。
    pub updated_at: DateTime<Utc>,
    /// 完成时间。
    pub finished_at: Option<DateTime<Utc>>,
    /// 结果图片。
    pub images: Vec<TaskImage>,
    /// 失败原因（对外文案）。
    pub error: Option<String>,
    /// 当前阶段（排队中 / 生成中 / 已完成 / 已中断）。
    pub progress: String,
    /// 耗时（毫秒，完成时结算）。
    pub duration_ms: Option<i64>,
    /// 参考图折算的输入 token。
    pub input_tokens: i64,
    /// 生成图片折算的输出 token。
    pub output_tokens: i64,
}

struct Inner {
    tasks: HashMap<Uuid, ImageTask>,
    order: VecDeque<Uuid>,
}

/// 内存任务存储：容量上限 + 终态保留期，超了先淘汰最老的终态任务。
///
/// 容量与保留期是写死常量：任务本身是一次性的，不值得为它开设置项；
/// 真正需要调的是并发上限（`image_task_concurrency`）。
pub struct ImageTaskStore {
    inner: Mutex<Inner>,
    limiter: Arc<Semaphore>,
    capacity: usize,
    retention: Duration,
}

impl ImageTaskStore {
    /// 用并发上限构造（并发由设置决定，启动时读一次）。
    pub fn new(concurrency: usize) -> Self {
        let concurrency = concurrency.clamp(1, 64);
        Self {
            inner: Mutex::new(Inner {
                tasks: HashMap::new(),
                order: VecDeque::new(),
            }),
            limiter: Arc::new(Semaphore::new(concurrency)),
            capacity: 256,
            retention: Duration::from_secs(6 * 3600),
        }
    }

    /// 并发令牌（runner 执行前先拿）。
    pub fn limiter(&self) -> Arc<Semaphore> {
        self.limiter.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 插入一个新任务，顺带淘汰过期与超量。
    ///
    /// 先插入再修剪：容量按插入后的总数判断，否则永远差一个（淘汰看的是插入前的长度）。
    pub fn insert(&self, task: ImageTask) {
        let mut inner = self.lock();
        let now = Utc::now();
        inner.order.push_back(task.id);
        inner.tasks.insert(task.id, task);
        Self::evict_locked(&mut inner, self.capacity, self.retention, now);
    }

    fn evict_locked(inner: &mut Inner, capacity: usize, retention: Duration, now: DateTime<Utc>) {
        // 先按保留期淘汰终态任务
        let keep_after = now - chrono::Duration::from_std(retention).unwrap_or_default();
        let expired: Vec<Uuid> = inner
            .tasks
            .values()
            .filter(|task| task.status.is_terminal())
            .filter(|task| task.updated_at < keep_after)
            .map(|task| task.id)
            .collect();
        for id in &expired {
            inner.tasks.remove(id);
        }
        if !expired.is_empty() {
            inner.order.retain(|id| !expired.contains(id));
        }
        // 再按容量淘汰最老的终态任务（不动运行中的）
        while inner.tasks.len() > capacity {
            let candidate = inner
                .order
                .iter()
                .find(|id| inner.tasks.get(id).map(|t| t.status.is_terminal()).unwrap_or(false))
                .copied();
            match candidate {
                Some(id) => {
                    inner.tasks.remove(&id);
                    inner.order.retain(|other| *other != id);
                }
                None => break,
            }
        }
    }

    /// 取一个任务（克隆，调用方不持有锁）。
    pub fn get(&self, id: Uuid) -> Option<ImageTask> {
        self.lock().tasks.get(&id).cloned()
    }

    /// 列出任务；给了 `ids` 就只取这些。按创建时间倒序。
    pub fn list(&self, ids: Option<&[Uuid]>) -> Vec<ImageTask> {
        let inner = self.lock();
        let mut out: Vec<ImageTask> = match ids {
            Some(ids) => ids
                .iter()
                .filter_map(|id| inner.tasks.get(id).cloned())
                .collect(),
            None => inner.tasks.values().cloned().collect(),
        };
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        out
    }

    /// 删除一个任务（任何状态都可删；运行中的只是摘记录，上游请求继续跑完丢弃）。
    pub fn delete(&self, id: Uuid) -> bool {
        let mut inner = self.lock();
        let removed = inner.tasks.remove(&id).is_some();
        if removed {
            inner.order.retain(|other| *other != id);
        }
        removed
    }

    /// 启动时把库里的任务灌进内存（重启后历史仍可见）。
    pub fn hydrate(&self, task: ImageTask) {
        let mut inner = self.lock();
        inner.order.push_back(task.id);
        inner.tasks.insert(task.id, task);
        Self::evict_locked(&mut inner, self.capacity, self.retention, Utc::now());
    }

    /// 置为运行中；任务已被删除或不是 queued 则返回 false。
    pub fn mark_running(&self, id: Uuid) -> bool {
        let mut inner = self.lock();
        let Some(task) = inner.tasks.get_mut(&id) else {
            return false;
        };
        if task.status != TaskStatus::Queued {
            return false;
        }
        task.status = TaskStatus::Running;
        task.progress = "生成中".to_string();
        task.updated_at = Utc::now();
        true
    }

    /// 更新阶段文案（任务已被删除则忽略）。
    pub fn set_progress(&self, id: Uuid, progress: &str) {
        let mut inner = self.lock();
        if let Some(task) = inner.tasks.get_mut(&id) {
            task.progress = progress.to_string();
            task.updated_at = Utc::now();
        }
    }

    /// 写入终态；任务已被删除则返回 `None`（结果被丢弃）。
    ///
    /// 返回结算后的快照，调用方拿它落库（耗时、token 都在里面）。
    pub fn finish(
        &self,
        id: Uuid,
        outcome: Result<Vec<TaskImage>, String>,
        input_tokens: i64,
        output_tokens: i64,
    ) -> Option<ImageTask> {
        let mut inner = self.lock();
        let task = inner.tasks.get_mut(&id)?;
        let now = Utc::now();
        task.updated_at = now;
        task.finished_at = Some(now);
        task.duration_ms = Some((now - task.created_at).num_milliseconds().max(0));
        task.input_tokens = input_tokens;
        task.output_tokens = output_tokens;
        match outcome {
            Ok(images) => {
                task.status = TaskStatus::Succeeded;
                task.progress = "已完成".to_string();
                task.images = images;
            }
            Err(message) => {
                task.status = TaskStatus::Failed;
                task.progress = "已失败".to_string();
                task.error = Some(message);
            }
        }
        Some(task.clone())
    }
}

/// 库里的行还原成内存任务（重启后把历史灌回来）。
pub fn task_from_row(row: &gpt2apirs_storage::repo::ImageTaskRow) -> ImageTask {
    let status = match row.status.as_str() {
        "queued" => TaskStatus::Queued,
        "running" => TaskStatus::Running,
        "succeeded" => TaskStatus::Succeeded,
        _ => TaskStatus::Failed,
    };
    let images: Vec<gpt2apirs_storage::repo::TaskImageRow> =
        serde_json::from_value(row.images.clone()).unwrap_or_default();
    ImageTask {
        id: row.id,
        status,
        prompt: row.prompt.clone(),
        model: row.model.clone(),
        n: row.n.max(1) as u32,
        response_format: row.response_format.clone(),
        output_format: row.output_format.clone(),
        frame_count: row.frame_count.map(|value| value as u32),
        frame_duration_ms: row.frame_duration_ms.map(|value| value as u32),
        base_url: row.base_url.clone(),
        created_at: row.created_at,
        updated_at: row.updated_at,
        finished_at: row.finished_at,
        images: images
            .into_iter()
            .map(|image| TaskImage {
                mime: leak_mime(&image.mime),
                rel_path: image.rel_path,
                width: image.width.max(0) as u32,
                height: image.height.max(0) as u32,
            })
            .collect(),
        error: row.error.clone(),
        progress: row.progress.clone().unwrap_or_else(|| "未知".to_string()),
        duration_ms: row.duration_ms,
        input_tokens: row.input_tokens.unwrap_or(0),
        output_tokens: row.output_tokens.unwrap_or(0),
    }
}

/// 任务结果里的 mime 是 `&'static str`；库里读出来是 String，这里归一化。
fn leak_mime(raw: &str) -> &'static str {
    match raw {
        "image/png" => "image/png",
        "image/jpeg" => "image/jpeg",
        "image/webp" => "image/webp",
        "image/gif" => "image/gif",
        _ => "image/png",
    }
}

/// 任务落库（失败只记日志：数据库抖一下不该让任务本身失败）。
async fn persist_new(state: &Arc<AppState>, task: &ImageTask) {
    let repo = gpt2apirs_storage::repo::ImageTaskRepo::new(state.db.clone());
    let record = gpt2apirs_storage::repo::NewImageTask {
        id: task.id,
        prompt: task.prompt.clone(),
        model: task.model.clone(),
        n: task.n as i32,
        response_format: task.response_format.clone(),
        output_format: task.output_format.clone(),
        frame_count: task.frame_count.map(|value| value as i32),
        frame_duration_ms: task.frame_duration_ms.map(|value| value as i32),
        base_url: task.base_url.clone(),
    };
    if let Err(error) = repo.insert(&record).await {
        tracing::warn!(%error, id = %task.id, "任务落库失败");
    }
}

/// 结算落库：进度、耗时、token、结果一起写回。
async fn persist_finish(state: &Arc<AppState>, task: &ImageTask) {
    let repo = gpt2apirs_storage::repo::ImageTaskRepo::new(state.db.clone());
    let images: Vec<gpt2apirs_storage::repo::TaskImageRow> = task
        .images
        .iter()
        .map(|image| gpt2apirs_storage::repo::TaskImageRow {
            rel_path: image.rel_path.clone(),
            mime: image.mime.to_string(),
            width: image.width as i32,
            height: image.height as i32,
        })
        .collect();
    let succeeded = task.status == TaskStatus::Succeeded;
    if let Err(error) = repo
        .finish(
            task.id,
            succeeded,
            &images,
            task.error.as_deref(),
            task.duration_ms.unwrap_or_default(),
            task.input_tokens,
            task.output_tokens,
        )
        .await
    {
        tracing::warn!(%error, id = %task.id, "任务结算落库失败");
    }
}

/// 提交任务并起后台执行，返回任务 id。
pub async fn submit(
    state: Arc<AppState>,
    task: ImageTask,
    request: gpt2apirs_generate::ImageRequest,
    key: Option<crate::auth::ApiKeyIdentity>,
) -> Uuid {
    let id = task.id;
    state.image_tasks.insert(task.clone());
    persist_new(&state, &task).await;
    tokio::spawn(run(state, id, request, key));
    id
}

/// 提交图生图任务并起后台执行（参考图字节随任务一起带去）。
pub async fn submit_edit(
    state: Arc<AppState>,
    task: ImageTask,
    request: gpt2apirs_generate::ImageRequest,
    prepared: Vec<(Vec<u8>, String)>,
    input_tokens: i64,
    key: Option<crate::auth::ApiKeyIdentity>,
) -> Uuid {
    let id = task.id;
    state.image_tasks.insert(task.clone());
    persist_new(&state, &task).await;
    tokio::spawn(run_edit(state, id, request, prepared, input_tokens, key));
    id
}

async fn run_edit(
    state: Arc<AppState>,
    id: Uuid,
    request: gpt2apirs_generate::ImageRequest,
    prepared: Vec<(Vec<u8>, String)>,
    input_tokens: i64,
    key: Option<crate::auth::ApiKeyIdentity>,
) {
    let store = state.image_tasks.clone();
    let Ok(_permit) = store.limiter().acquire_owned().await else {
        return;
    };
    if !store.mark_running(id) {
        return;
    }
    let prompt = request.prompt.clone();
    let deadline = gpt2apirs_generate::default_request_deadline(&state.settings);
    if let Err(error) = crate::routes::image_flow::ensure_disk_space() {
        if let Some(task) = store.finish(id, Err(error.error.message()), 0, 0) {
            persist_finish(&state, &task).await;
        }
        return;
    }
    match crate::routes::image_flow::run_edit(&state, request, prepared.clone(), deadline).await {
        Ok(slots) => {
            // 与同步接口一致：生成成功才落盘参考图并写清单。
            let references = crate::references::persist_all(&prepared);
            crate::routes::image_flow::record_assets(
                &state,
                &slots,
                &prompt,
                &references,
                key.as_ref(),
            )
            .await;
            let images: Vec<TaskImage> = slots
                .iter()
                .flat_map(|slot| slot.images.iter())
                .filter(|image| !image.rel_path.is_empty())
                .map(|image| TaskImage {
                    rel_path: image.rel_path.clone(),
                    mime: image.mime,
                    width: image.width,
                    height: image.height,
                })
                .collect();
            let output_tokens: i64 = images
                .iter()
                .map(|image| {
                    gpt2apirs_core::tokens::count_generated_image_tokens(
                        image.width,
                        image.height,
                        "auto",
                    )
                })
                .sum();
            // 上游只回了文字：任务没有地方放这句话，按失败收尾并把它当作原因
            let text = slots
                .iter()
                .map(|slot| slot.text.trim())
                .find(|text| !text.is_empty());
            let outcome = match (images.is_empty(), text) {
                (true, Some(text)) => Err(text.to_string()),
                _ => Ok(images),
            };
            let output_tokens = if matches!(outcome, Ok(_)) { output_tokens } else { 0 };
            if let Some(task) = store.finish(id, outcome, input_tokens, output_tokens) {
                persist_finish(&state, &task).await;
            }
        }
        Err(error) => {
            if let Some(task) = store.finish(id, Err(error.error.message()), input_tokens, 0) {
                persist_finish(&state, &task).await;
            }
        }
    }
}

async fn run(
    state: Arc<AppState>,
    id: Uuid,
    request: gpt2apirs_generate::ImageRequest,
    key: Option<crate::auth::ApiKeyIdentity>,
) {
    let store = state.image_tasks.clone();
    let Ok(permit) = store.limiter().acquire_owned().await else {
        return;
    };
    // 已被删除就别跑了
    if !store.mark_running(id) {
        return;
    }
    let prompt = request.prompt.clone();
    let quality = request
        .quality
        .clone()
        .unwrap_or_else(|| "auto".to_string());
    let deadline = gpt2apirs_generate::default_request_deadline(&state.settings);
    let outcome = match crate::routes::image_flow::run_generation(&state, request, deadline).await {
        Ok(slots) => {
            crate::routes::image_flow::record_assets(&state, &slots, &prompt, &[], key.as_ref()).await;
            let mut images = Vec::new();
            for slot in &slots {
                for image in &slot.images {
                    // 未落盘的图片没有文件可读，任务里也不记（否则会生成一个 404 的 URL）。
                    if image.rel_path.is_empty() {
                        continue;
                    }
                    images.push(TaskImage {
                        rel_path: image.rel_path.clone(),
                        mime: image.mime,
                        width: image.width,
                        height: image.height,
                    });
                }
            }
            // 上游只回了文字：任务没有地方放这句话，按失败收尾并把它当作原因
            match slots
                .iter()
                .map(|slot| slot.text.trim())
                .find(|text| !text.is_empty())
            {
                Some(text) if images.is_empty() => Err(text.to_string()),
                _ => Ok(images),
            }
        }
        Err(error) => Err(error.error.message()),
    };
    let output_tokens: i64 = match &outcome {
        Ok(images) => images
            .iter()
            .map(|image| {
                gpt2apirs_core::tokens::count_generated_image_tokens(
                    image.width,
                    image.height,
                    &quality,
                )
            })
            .sum(),
        Err(_) => 0,
    };
    if let Some(finished) = store.finish(id, outcome, 0, output_tokens) {
        persist_finish(&state, &finished).await;
    }
    drop(permit);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> ImageTask {
        let now = Utc::now();
        ImageTask {
            id: Uuid::new_v4(),
            status: TaskStatus::Queued,
            prompt: "画一只猫".to_string(),
            model: gpt2apirs_generate::models::DEFAULT_IMAGE_MODEL.to_string(),
            n: 1,
            response_format: "url".to_string(),
            output_format: None,
            frame_count: None,
            frame_duration_ms: None,
            base_url: "http://localhost:8080".to_string(),
            created_at: now,
            updated_at: now,
            finished_at: None,
            images: Vec::new(),
            error: None,
            progress: "排队中".to_string(),
            duration_ms: None,
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    #[test]
    fn lifecycle_queued_to_succeeded() {
        let store = ImageTaskStore::new(2);
        let t = task();
        let id = t.id;
        store.insert(t);
        assert_eq!(store.get(id).unwrap().status, TaskStatus::Queued);
        assert!(store.mark_running(id));
        assert_eq!(store.get(id).unwrap().status, TaskStatus::Running);
        let images = vec![TaskImage {
            rel_path: "images/x.png".to_string(),
            mime: "image/png",
            width: 1,
            height: 1,
        }];
        assert!(store.finish(id, Ok(images), 0, 0).is_some());
        let done = store.get(id).unwrap();
        assert_eq!(done.status, TaskStatus::Succeeded);
        assert_eq!(done.images.len(), 1);
        assert!(done.finished_at.is_some());
    }

    #[test]
    fn lifecycle_failed_sets_error() {
        let store = ImageTaskStore::new(1);
        let t = task();
        let id = t.id;
        store.insert(t);
        assert!(store.mark_running(id));
        assert!(store.finish(id, Err("上游超时".to_string()), 0, 0).is_some());
        let done = store.get(id).unwrap();
        assert_eq!(done.status, TaskStatus::Failed);
        assert_eq!(done.error.as_deref(), Some("上游超时"));
    }

    #[test]
    fn delete_removes_and_finish_noop() {
        let store = ImageTaskStore::new(1);
        let t = task();
        let id = t.id;
        store.insert(t);
        assert!(store.delete(id));
        assert!(store.get(id).is_none());
        // 删除后 finish 应是 no-op
        assert!(store.finish(id, Ok(Vec::new()), 0, 0).is_none(), "任务已删掉，结果应被丢弃");
        // 重复删除返回 false
        assert!(!store.delete(id));
    }

    #[test]
    fn mark_running_only_from_queued() {
        let store = ImageTaskStore::new(1);
        let t = task();
        let id = t.id;
        store.insert(t);
        assert!(store.mark_running(id));
        // 第二次不该成功
        assert!(!store.mark_running(id));
    }

    #[test]
    fn list_sorted_and_filtered() {
        let store = ImageTaskStore::new(1);
        let mut a = task();
        a.created_at = Utc::now() - chrono::Duration::seconds(10);
        let id_a = a.id;
        let b = task();
        let id_b = b.id;
        store.insert(a);
        store.insert(b);
        let all = store.list(None);
        assert_eq!(all.len(), 2);
        // 倒序：b 更新在前
        assert_eq!(all[0].id, id_b);
        let only = store.list(Some(&[id_a]));
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].id, id_a);
    }

    #[test]
    fn capacity_evicts_oldest_terminal() {
        let mut store = ImageTaskStore::new(1);
        store.capacity = 2;
        let mut first = task();
        first.created_at = Utc::now() - chrono::Duration::seconds(10);
        let id_first = first.id;
        store.insert(first);
        store.mark_running(id_first);
        store.finish(id_first, Ok(Vec::new()), 0, 0);
        store.insert(task());
        // 第三个触发淘汰最老终态
        store.insert(task());
        assert!(store.get(id_first).is_none(), "最老终态应被淘汰");
        assert_eq!(store.list(None).len(), 2);
    }
}
