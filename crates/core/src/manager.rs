//! 任务管理器：注册表、并发调度、全局统计、事件广播。
//!
//! 对应原版的 `TASK` 静态类 + 主窗体里的任务表，但去掉了全局可变状态：
//! 所有操作都通过 `Arc<Manager>` 进行，UI 与 RPC 共用同一份实例。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, RwLock};
use tokio::sync::broadcast;

use crate::error::{Error, Result};
use crate::task::{Event, Gid, Snapshot, Task, TaskOptions, TaskStatus};

#[derive(Debug, Clone)]
pub struct ManagerConfig {
    /// 同时下载的任务数（aria2 的 `max-concurrent-downloads`）。
    pub max_concurrent: usize,
    pub default_dir: PathBuf,
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 5,
            default_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }
}

pub struct GlobalStat {
    pub download_speed: u64,
    pub num_active: usize,
    pub num_waiting: usize,
    pub num_stopped: usize,
}

pub struct Manager {
    tasks: RwLock<BTreeMap<Gid, Arc<Task>>>,
    /// 任务顺序（添加顺序，用于 tellWaiting / changePosition）。
    order: Mutex<Vec<Gid>>,
    events: broadcast::Sender<Event>,
    cfg: ManagerConfig,
    client: reqwest::Client,
    next_gid: AtomicU64,
    starting: AtomicUsize,
    /// 已完成/失败/删除的任务保留在表中，供 tellStopped 查询。
    ///
    /// 预留：接入 aria2 的 `max-download-result` 淘汰策略（超出后丢弃最旧的
    /// 终态任务）。当前 stopped 表随会话增长，长跑场景需要这个上限。
    #[allow(dead_code)]
    max_results: AtomicUsize,
}

impl Manager {
    pub async fn new(cfg: ManagerConfig, client: reqwest::Client) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x2089_b05e_cca3_d829)
            & 0xffff_0000_0000_0000;

        let mgr = Arc::new(Self {
            tasks: RwLock::new(BTreeMap::new()),
            order: Mutex::new(Vec::new()),
            events,
            cfg,
            client,
            next_gid: AtomicU64::new(seed | 1),
            starting: AtomicUsize::new(0),
            max_results: AtomicUsize::new(1000),
        });

        // 速度采样：每秒一次，避免每个 RPC 请求都重算
        let weak = Arc::downgrade(&mgr);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let Some(m) = weak.upgrade() else { return };
                m.sample_speeds();
            }
        });

        mgr
    }

    fn sample_speeds(&self) {
        for t in self.tasks.read().values() {
            if t.status() == TaskStatus::Active {
                t.sample_speed();
            }
        }
    }

    /// 事件广播发送端（RPC 的 WebSocket 通知需要）。
    pub fn events(&self) -> broadcast::Sender<Event> {
        self.events.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn config(&self) -> &ManagerConfig {
        &self.cfg
    }

    /// 新增任务。等价于 aria2 的 `aria2.addUri`。
    pub fn add_uri(self: &Arc<Self>, uris: Vec<String>, opts: TaskOptions) -> Result<Gid> {
        if uris.is_empty() {
            return Err(Error::BadRequest("uris 不能为空".into()));
        }
        for u in &uris {
            url::Url::parse(u).map_err(|e| Error::InvalidUrl(format!("{u}: {e}")))?;
        }

        let gid = self.next_gid.fetch_add(1, Ordering::Relaxed);
        let task = Arc::new(Task::new(gid, uris, opts, self.events.clone()));

        self.tasks.write().insert(gid, task.clone());
        self.order.lock().push(gid);
        self.schedule();
        Ok(gid)
    }

    pub fn get(&self, gid: Gid) -> Option<Arc<Task>> {
        self.tasks.read().get(&gid).cloned()
    }

    pub fn find(&self, gid_hex: &str) -> Result<Arc<Task>> {
        let gid = crate::util::gid_from_hex(gid_hex)
            .ok_or_else(|| Error::TaskNotFound(gid_hex.to_string()))?;
        self.get(gid)
            .ok_or_else(|| Error::TaskNotFound(gid_hex.to_string()))
    }

    /// 按添加顺序返回全部任务。
    pub fn all(&self) -> Vec<Arc<Task>> {
        let order = self.order.lock();
        let tasks = self.tasks.read();
        order.iter().filter_map(|g| tasks.get(g).cloned()).collect()
    }

    pub fn active(&self) -> Vec<Arc<Task>> {
        self.all()
            .into_iter()
            .filter(|t| t.status() == TaskStatus::Active)
            .collect()
    }

    pub fn waiting(&self) -> Vec<Arc<Task>> {
        self.all()
            .into_iter()
            .filter(|t| t.status() == TaskStatus::Waiting)
            .collect()
    }

    pub fn stopped(&self) -> Vec<Arc<Task>> {
        self.all()
            .into_iter()
            .filter(|t| {
                matches!(
                    t.status(),
                    TaskStatus::Complete | TaskStatus::Error | TaskStatus::Removed
                )
            })
            .collect()
    }

    pub fn snapshots(&self, list: &[Arc<Task>]) -> Vec<Snapshot> {
        list.iter().map(|t| t.snapshot()).collect()
    }

    pub fn global_stat(&self) -> GlobalStat {
        let all = self.all();
        let mut speed = 0u64;
        let (mut active, mut waiting, mut stopped) = (0, 0, 0);
        for t in &all {
            match t.status() {
                TaskStatus::Active => {
                    active += 1;
                    speed += t.snapshot().speed;
                }
                TaskStatus::Waiting => waiting += 1,
                TaskStatus::Complete | TaskStatus::Error | TaskStatus::Removed => stopped += 1,
                TaskStatus::Paused => {}
            }
        }
        GlobalStat {
            download_speed: speed,
            num_active: active,
            num_waiting: waiting,
            num_stopped: stopped,
        }
    }

    pub fn pause(&self, gid: Gid) -> Result<()> {
        let t = self.get(gid).ok_or(Error::TaskNotFound(gid.to_string()))?;
        match t.status() {
            TaskStatus::Active | TaskStatus::Waiting => {
                t.request_stop(TaskStatus::Paused);
                Ok(())
            }
            _ => Err(Error::BadRequest(format!(
                "任务状态不可暂停: {}",
                t.status().as_aria2()
            ))),
        }
    }

    pub fn unpause(self: &Arc<Self>, gid: Gid) -> Result<()> {
        let t = self.get(gid).ok_or(Error::TaskNotFound(gid.to_string()))?;
        if t.status() != TaskStatus::Paused {
            return Err(Error::BadRequest(format!(
                "任务状态不可恢复: {}",
                t.status().as_aria2()
            )));
        }
        t.resume_flag();
        self.schedule();
        Ok(())
    }

    pub fn pause_all(&self) {
        for t in self.all() {
            if matches!(t.status(), TaskStatus::Active | TaskStatus::Waiting) {
                t.request_stop(TaskStatus::Paused);
            }
        }
    }

    pub fn unpause_all(self: &Arc<Self>) {
        for t in self.all() {
            if t.status() == TaskStatus::Paused {
                t.resume_flag();
            }
        }
        self.schedule();
    }

    /// 删除任务。`force` 时连进度文件一并删除。
    pub fn remove(&self, gid: Gid, force: bool) -> Result<()> {
        let t = self.get(gid).ok_or(Error::TaskNotFound(gid.to_string()))?;
        if matches!(t.status(), TaskStatus::Active | TaskStatus::Waiting | TaskStatus::Paused) {
            t.request_stop(TaskStatus::Removed);
        }
        if force {
            let _ = std::fs::remove_file(t.part_path());
            let _ = std::fs::remove_file(t.meta_path());
        }
        Ok(())
    }

    pub fn remove_download_result(&self, gid: Gid) {
        self.tasks.write().remove(&gid);
        self.order.lock().retain(|g| *g != gid);
    }

    pub fn purge_download_result(&self) {
        let done: Vec<Gid> = self.stopped().iter().map(|t| t.gid).collect();
        for gid in done {
            self.remove_download_result(gid);
        }
    }

    pub fn change_position(&self, gid: Gid, pos: i64, how: &str) -> Result<()> {
        let mut order = self.order.lock();
        let Some(cur) = order.iter().position(|g| *g == gid) else {
            return Err(Error::TaskNotFound(gid.to_string()));
        };
        let item = order.remove(cur);
        let target = match how {
            "POS_SET" => pos.clamp(0, order.len() as i64) as usize,
            "POS_CUR" => (cur as i64 + pos).clamp(0, order.len() as i64) as usize,
            "POS_END" => (order.len() as i64 + pos).clamp(0, order.len() as i64) as usize,
            _ => return Err(Error::BadRequest(format!("未知的 how: {how}"))),
        };
        let idx = target.min(order.len());
        order.insert(idx, item);
        Ok(())
    }

    /// 调度：把 waiting 的任务拉起来，直到达到并发上限。
    fn schedule(self: &Arc<Self>) {
        loop {
            let active = self.starting.load(Ordering::SeqCst);
            if active >= self.cfg.max_concurrent {
                return;
            }
            let next = self
                .all()
                .into_iter()
                .find(|t| t.status() == TaskStatus::Waiting && t.claim_start());
            let Some(task) = next else { return };

            self.starting.fetch_add(1, Ordering::SeqCst);
            let mgr = self.clone();
            let client = self.client.clone();
            tokio::spawn(async move {
                let _ = task.clone().run(client).await;
                mgr.starting.fetch_sub(1, Ordering::SeqCst);
                mgr.schedule();
            });
        }
    }

    /// 供 UI 手动触发（例如 unpause 后）。
    pub fn kick(self: &Arc<Self>) {
        self.schedule();
    }

    pub fn shutdown(&self) {
        for t in self.all() {
            if matches!(t.status(), TaskStatus::Active | TaskStatus::Waiting) {
                t.request_stop(TaskStatus::Removed);
            }
        }
    }
}
