//! 单任务状态机与执行。
//!
//! 并发模型（对照原版）：
//! ```text
//!   [worker 0..N]  --take-->  待下载区间
//!        |                        ^
//!        | Range 请求流式读         | put（未完成的剩余部分，Drop 兜底）
//!        v                        |
//!     写通道(mpsc) ---------> [唯一 writer] --pwrite--> 文件
//!                                  |
//!                                  +--> commit(区间) 仅在此处发生
//! ```
//!
//! 三条硬性约束（原版全部违反）：
//! 1. **只有 pwrite 成功**才 `commit`，进度按已落盘字节计（F-01）
//! 2. 任何异常/取消路径，未完成区间由 [`RangeLease`] 的 Drop 自动归还（F-02）
//! 3. 续传前校验 ETag / 长度指纹（F-03）

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::error::{Error, Result};
use crate::probe::{self, Fingerprint, Probe};
use crate::range::{PendingRanges, Segment};
use crate::resume::{ResumeMeta, SaveThrottle};
use crate::util::{gid_to_hex, sanitize_filename, size_str};
use crate::writer::ChunkWriter;

/// 单块读写缓冲（2 MB）。介于「足够大以减少系统调用」与「多连接下内存可控」之间。
pub const CHUNK: u64 = 2 * 1024 * 1024;

/// aria2 的 piece-length 默认值，用于生成 bitfield。
pub const PIECE_LEN: u64 = 1024 * 1024;

/// 速度滑动窗口：取最近 5 秒的样本算平均，兼顾平滑与响应。
pub const SPEED_WINDOW: Duration = Duration::from_secs(5);

pub type Gid = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Waiting,
    Active,
    Paused,
    Complete,
    Error,
    Removed,
}

impl TaskStatus {
    /// aria2 状态字符串。
    pub fn as_aria2(&self) -> &'static str {
        match self {
            TaskStatus::Waiting => "waiting",
            TaskStatus::Active => "active",
            TaskStatus::Paused => "paused",
            TaskStatus::Complete => "complete",
            TaskStatus::Error => "error",
            TaskStatus::Removed => "removed",
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, TaskStatus::Complete | TaskStatus::Error | TaskStatus::Removed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskOptions {
    pub dir: PathBuf,
    pub out: Option<String>,
    /// 连接数（aria2 的 `split`）。
    pub split: usize,
    /// 最小分片（aria2 的 `min-split-size`）。
    pub min_split_size: u64,
    pub referer: Option<String>,
    pub user_agent: Option<String>,
    pub headers: Vec<(String, String)>,
    /// 添加后是否立即暂停。
    pub pause: bool,
    /// 原始 aria2 选项，便于 `getOption` 原样回显。
    pub raw: BTreeMap<String, String>,
}

impl Default for TaskOptions {
    fn default() -> Self {
        Self {
            dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            out: None,
            split: 5,
            min_split_size: 4 * 1024 * 1024,
            referer: None,
            user_agent: None,
            headers: Vec::new(),
            pause: false,
            raw: BTreeMap::new(),
        }
    }
}

impl TaskOptions {
    pub fn extra_headers(&self) -> Vec<(String, String)> {
        let mut h = Vec::new();
        if let Some(r) = &self.referer {
            h.push(("Referer".to_string(), r.clone()));
        }
        if let Some(ua) = &self.user_agent {
            h.push(("User-Agent".to_string(), ua.clone()));
        }
        h.extend(self.headers.iter().cloned());
        h
    }
}

/// 事件（桥接 aria2 的 WebSocket 通知）。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event")]
pub enum Event {
    Start { gid: String },
    Pause { gid: String },
    Stop { gid: String },
    Complete { gid: String },
    Error { gid: String },
}

/// 对外快照（UI / RPC 共用）。
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub gid: Gid,
    pub status: TaskStatus,
    pub total: Option<u64>,
    pub completed: u64,
    pub speed: u64,
    pub dir: PathBuf,
    pub filename: String,
    pub uris: Vec<String>,
    pub connections: usize,
    pub error: Option<String>,
    pub error_code: i32,
    pub bitfield: Vec<u8>,
    pub num_pieces: u64,
    pub piece_len: u64,
}

struct Inner {
    status: TaskStatus,
    total: Option<u64>,
    completed: u64,
    speed: u64,
    filename: String,
    connections: usize,
    error: Option<String>,
    error_code: i32,
    fingerprint: Option<Fingerprint>,
}

pub struct Task {
    pub gid: Gid,
    pub uris: Vec<String>,
    pub opts: TaskOptions,
    /// 剩余待下载。
    remaining: Mutex<PendingRanges>,
    /// 已完成（0-based 半开，自动合并）。
    done: Mutex<PendingRanges>,
    inner: Mutex<Inner>,
    /// 取消令牌。暂停/恢复会整体替换（旧令牌一旦 cancel 不可复用）。
    cancel: RwLock<CancellationToken>,
    speed_meter: Mutex<SpeedMeter>,
    claimed: AtomicBool,
    events: broadcast::Sender<Event>,
    /// 进度计数，供速度采样无锁读取。
    completed_atomic: AtomicU64,
    /// 任务创建时刻。预留：RPC 的终态任务按创建时间淘汰时排序用。
    #[allow(dead_code)]
    created_at: Instant,
    throttle: Mutex<SaveThrottle>,
}

impl Task {
    pub fn new(
        gid: Gid,
        uris: Vec<String>,
        opts: TaskOptions,
        events: broadcast::Sender<Event>,
    ) -> Self {
        let status = if opts.pause {
            TaskStatus::Paused
        } else {
            TaskStatus::Waiting
        };
        let filename = opts
            .out
            .clone()
            .unwrap_or_else(|| "download".to_string());
        Self {
            gid,
            uris,
            opts,
            remaining: Mutex::new(PendingRanges::new()),
            done: Mutex::new(PendingRanges::new()),
            inner: Mutex::new(Inner {
                status,
                total: None,
                completed: 0,
                speed: 0,
                filename,
                connections: 0,
                error: None,
                error_code: 0,
                fingerprint: None,
            }),
            cancel: RwLock::new(CancellationToken::new()),
            speed_meter: Mutex::new(SpeedMeter::default()),
            claimed: AtomicBool::new(false),
            events,
            completed_atomic: AtomicU64::new(0),
            created_at: Instant::now(),
            throttle: Mutex::new(SaveThrottle::new(Duration::from_secs(2), 64 << 20)),
        }
    }

    pub fn gid_hex(&self) -> String {
        gid_to_hex(self.gid)
    }

    /// 取当前取消令牌（worker 内部应持有一份副本，避免恢复后串号）。
    pub fn token(&self) -> CancellationToken {
        self.cancel.read().clone()
    }

    /// 抢占启动权：保证一个任务只会被调度一次。
    pub fn claim_start(&self) -> bool {
        self.claimed
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    /// 从 Paused 恢复：换新令牌、回到 Waiting、允许再次抢占。
    pub fn resume_flag(&self) {
        *self.cancel.write() = CancellationToken::new();
        self.claimed.store(false, Ordering::SeqCst);
        self.inner.lock().status = TaskStatus::Waiting;
    }

    /// 速度采样（由管理器每秒调用）。
    pub fn sample_speed(&self) {
        let bytes = self.completed();
        let mut m = self.speed_meter.lock();
        if let Some(v) = m.sample(bytes) {
            self.inner.lock().speed = v;
        }
    }

    pub fn status(&self) -> TaskStatus {
        self.inner.lock().status
    }

    fn set_status(&self, s: TaskStatus) {
        self.inner.lock().status = s;
        let ev = match s {
            TaskStatus::Active => Event::Start { gid: self.gid_hex() },
            TaskStatus::Paused => Event::Pause { gid: self.gid_hex() },
            TaskStatus::Complete => Event::Complete { gid: self.gid_hex() },
            TaskStatus::Error => Event::Error { gid: self.gid_hex() },
            TaskStatus::Removed => Event::Stop { gid: self.gid_hex() },
            TaskStatus::Waiting => return,
        };
        let _ = self.events.send(ev);
    }

    pub fn snapshot(&self) -> Snapshot {
        let inner = self.inner.lock();
        let done = self.done.lock();
        let total = inner.total;
        let piece_len = PIECE_LEN;
        let num_pieces = total.map(|t| t.div_ceil(piece_len)).unwrap_or(0);
        Snapshot {
            gid: self.gid,
            status: inner.status,
            total,
            completed: inner.completed,
            speed: inner.speed,
            dir: self.opts.dir.clone(),
            filename: inner.filename.clone(),
            uris: self.uris.clone(),
            connections: inner.connections,
            error: inner.error.clone(),
            error_code: inner.error_code,
            bitfield: build_bitfield(done.segments(), total, piece_len),
            num_pieces,
            piece_len: if num_pieces > 0 { piece_len } else { 0 },
        }
    }

    pub fn done_segments(&self) -> Vec<Segment> {
        self.done.lock().segments()
    }

    pub fn completed(&self) -> u64 {
        self.completed_atomic.load(Ordering::Relaxed)
    }

    /// 目标文件名（清洗后）。
    pub fn final_path(&self) -> PathBuf {
        self.opts.dir.join(&self.inner.lock().filename)
    }

    pub fn part_path(&self) -> PathBuf {
        let mut s = self.final_path().into_os_string();
        s.push(".acgdev.part");
        PathBuf::from(s)
    }

    pub fn meta_path(&self) -> PathBuf {
        let mut s = self.final_path().into_os_string();
        s.push(".acgdev.json");
        PathBuf::from(s)
    }

    // ---- 区间操作 ----------------------------------------------------

    fn take_segment(&self, max_len: u64) -> Option<Segment> {
        self.remaining.lock().take(max_len)
    }

    fn put_segment(&self, seg: Segment) {
        self.remaining.lock().put(seg.0, seg.1);
    }

    /// 仅在数据真正落盘后调用。
    fn commit(&self, seg: Segment) {
        self.done.lock().put(seg.0, seg.1);
        let completed = self.done.lock().covered_len();
        self.completed_atomic.store(completed, Ordering::Relaxed);
        let mut inner = self.inner.lock();
        inner.completed = completed;
    }

    fn save_meta(&self, force: bool) -> Result<()> {
        let completed = self.completed();
        if !self.throttle.lock().should_save(completed, force) {
            return Ok(());
        }
        let fp = self.inner.lock().fingerprint.clone().unwrap_or_default();
        let meta = ResumeMeta::new(
            self.uris.first().cloned().unwrap_or_default(),
            &fp,
            self.done_segments(),
        );
        meta.save_atomic(&self.meta_path())
    }

    /// 用户主动暂停/删除：保存进度并取消。
    pub fn request_stop(&self, status: TaskStatus) {
        self.token().cancel();
        let _ = self.save_meta(true);
        self.set_status(status);
    }

    // ---- 执行 --------------------------------------------------------

    /// 执行下载直到完成 / 失败 / 取消。
    pub async fn run(self: Arc<Self>, client: reqwest::Client) -> Result<()> {
        if self.status() != TaskStatus::Waiting {
            return Ok(());
        }
        self.set_status(TaskStatus::Active);

        let result = self.run_inner(&client).await;

        match &result {
            Ok(()) => {
                self.inner.lock().speed = 0;
                if self.meta_path().exists() {
                    let _ = std::fs::remove_file(self.meta_path());
                }
                self.set_status(TaskStatus::Complete);
            }
            Err(Error::Cancelled) => {
                self.inner.lock().speed = 0;
                // 状态已由 request_stop 设置，这里不覆盖
            }
            Err(e) => {
                tracing::warn!(gid = %self.gid_hex(), "任务失败: {e}");
                let mut inner = self.inner.lock();
                inner.speed = 0;
                inner.error = Some(e.to_string());
                inner.error_code = error_code_of(e);
                drop(inner);
                let _ = self.save_meta(true);
                self.set_status(TaskStatus::Error);
            }
        }
        result
    }

    async fn run_inner(self: &Arc<Self>, client: &reqwest::Client) -> Result<()> {
        let url = Url::parse(&self.uris[0]).map_err(|e| Error::InvalidUrl(e.to_string()))?;
        let headers = self.opts.extra_headers();

        // 1) 探测
        let probe = probe::probe(client, &url, &headers).await?;
        let fp = probe.fingerprint();

        if let Some(name) = &probe.filename {
            if self.opts.out.is_none() {
                self.inner.lock().filename = name.clone();
            }
        }
        self.inner.lock().total = probe.total_len;
        self.inner.lock().fingerprint = Some(fp.clone());

        // 2) 续传：校验并恢复
        let resumed = self.try_resume(&fp)?;
        if let Some(total) = probe.total_len {
            if !resumed {
                *self.remaining.lock() = PendingRanges::full(total);
                *self.done.lock() = PendingRanges::new();
            }
            let already = self.done.lock().covered_len();
            self.inner.lock().completed = already;
            self.completed_atomic.store(already, Ordering::Relaxed);
            if already > 0 {
                tracing::info!(
                    gid = %self.gid_hex(),
                    "续传：已完成 {}/{}",
                    size_str(already),
                    size_str(total)
                );
            }
            if already >= total {
                self.finish_file(Some(total))?;
                return Ok(());
            }
        }

        let total = probe.total_len;
        let can_parallel = probe.accept_ranges && total.is_some() && self.opts.split > 1;

        if can_parallel {
            self.run_parallel(client, &url, &headers, total.unwrap()).await?;
        } else {
            self.run_single(client, &url, &headers, &probe).await?;
        }

        // 3) 收尾：校验 + 落盘 + 改名
        self.inner.lock().connections = 0;
        if total.is_some() || !probe.accept_ranges {
            self.finish_file(total)?;
        }
        self.save_meta(true)?;
        Ok(())
    }

    fn try_resume(&self, fp: &Fingerprint) -> Result<bool> {
        let path = self.meta_path();
        let meta = match ResumeMeta::load(&path) {
            Ok(Some(m)) => m,
            Ok(None) => return Ok(false),
            Err(Error::Corrupt(msg)) => {
                // 元数据损坏：安全起见从头下载，但保留备份不 panic
                tracing::warn!("续传元数据损坏，将重新下载: {msg}");
                let _ = std::fs::remove_file(&path);
                return Ok(false);
            }
            Err(e) => return Err(e),
        };

        // F-03：指纹不一致 → 拒绝续传
        meta.fingerprint().ensure_compatible(fp)?;

        if !self.part_path().exists() {
            return Ok(false);
        }
        *self.done.lock() = {
            let mut d = PendingRanges::new();
            for &(a, b) in &meta.done {
                d.put(a, b);
            }
            d
        };
        if let Some(total) = meta.total.or(fp.total) {
            *self.remaining.lock() = PendingRanges::from_done(total, &meta.done);
        }
        Ok(true)
    }

    async fn run_parallel(
        self: &Arc<Self>,
        client: &reqwest::Client,
        url: &Url,
        headers: &[(String, String)],
        total: u64,
    ) -> Result<()> {
        let writer = ChunkWriter::create_or_open(self.part_path(), Some(total))?;
        let writer = Arc::new(writer);

        let (tx, mut rx) = mpsc::channel::<WriteCmd>(self.opts.split * 2);

        let this = self.clone();
        let writer_task = tokio::spawn(async move {
            while let Some(cmd) = rx.recv().await {
                let res = writer.write_at(cmd.offset, &cmd.data);
                match &res {
                    Ok(()) => {
                        // 只有这里才推进进度
                        this.commit(cmd.seg);
                        let _ = this.save_meta(false);
                    }
                    Err(e) => {
                        tracing::error!("写入失败: {e}");
                        this.token().cancel();
                    }
                }
                let _ = cmd.ack.send(res.is_ok());
            }
        });

        let chunk = self.chunk_size(total);
        let mut workers = Vec::new();
        for idx in 0..self.opts.split {
            let this = self.clone();
            let client = client.clone();
            let url = url.clone();
            let headers = headers.to_vec();
            let tx = tx.clone();
            workers.push(tokio::spawn(async move {
                this.worker_loop(idx, client, url, headers, chunk, tx).await
            }));
        }
        drop(tx);

        let mut first_err: Option<Error> = None;
        for w in workers {
            match w.await {
                Ok(Err(e)) if first_err.is_none() => first_err = Some(e),
                Ok(_) => {}
                Err(join) => {
                    if first_err.is_none() {
                        first_err = Some(Error::Other(format!("worker 崩溃: {join}")));
                    }
                }
            }
        }
        let _ = writer_task.await;

        if self.token().is_cancelled() {
            self.save_meta(true)?;
            return Err(Error::Cancelled);
        }
        if let Some(e) = first_err {
            self.save_meta(true)?;
            return Err(e);
        }
        Ok(())
    }

    fn chunk_size(&self, total: u64) -> u64 {
        // 连接数很多时避免内存爆掉；同时不低于 256 KB 以免小块写放大。
        let by_split = (total / self.opts.split.max(1) as u64).max(256 * 1024);
        CHUNK.min(by_split).max(256 * 1024)
    }

    async fn worker_loop(
        self: Arc<Self>,
        _idx: usize,
        client: reqwest::Client,
        url: Url,
        headers: Vec<(String, String)>,
        chunk: u64,
        tx: mpsc::Sender<WriteCmd>,
    ) -> Result<()> {
        let token = self.token();
        loop {
            if token.is_cancelled() {
                return Ok(());
            }
            let Some(seg) = self.take_segment(chunk) else {
                return Ok(());
            };
            // 租约：任何提前返回都会把未写完的部分归还（F-02）
            let mut lease = RangeLease::new(self.clone(), seg);

            let mut req = client
                .get(url.clone())
                .header(reqwest::header::RANGE, format!("bytes={}-{}", seg.0, seg.1 - 1))
                .header(reqwest::header::ACCEPT_ENCODING, "identity");
            for (k, v) in &headers {
                req = req.header(k, v);
            }

            let resp = tokio::select! {
                _ = token.cancelled() => return Ok(()),
                r = req.send() => r?,
            };

            if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
                // 探测说支持 Range，这里却不返回 206：视为不支持
                return Err(Error::NoRangeSupport);
            }

            self.inner.lock().connections += 1;
            {
                let mut stream = resp.bytes_stream();
                let mut buf = BytesMut::with_capacity(chunk as usize);
                let mut offset = seg.0;

                loop {
                    let piece = tokio::select! {
                        _ = token.cancelled() => break,
                        p = stream.next() => p,
                    };
                    let Some(piece) = piece else { break };
                    let piece = piece?;
                    buf.extend_from_slice(&piece);

                    while buf.len() >= chunk as usize {
                        let data = buf.split_to(chunk as usize).freeze();
                        self.dispatch_write(&tx, offset, data, chunk).await?;
                        offset += chunk;
                        lease.advance(chunk);
                    }
                }
                if !buf.is_empty() {
                    let n = buf.len() as u64;
                    let data = buf.freeze();
                    self.dispatch_write(&tx, offset, data, n).await?;
                    lease.advance(n);
                }
            }
            self.inner.lock().connections -= 1;
            lease.disarm();
        }
    }

    async fn dispatch_write(
        &self,
        tx: &mpsc::Sender<WriteCmd>,
        offset: u64,
        data: Bytes,
        len: u64,
    ) -> Result<()> {
        let (ack_tx, ack_rx) = oneshot::channel();
        let cmd = WriteCmd {
            offset,
            data,
            seg: (offset, offset + len),
            ack: ack_tx,
        };
        if tx.send(cmd).await.is_err() {
            return Err(Error::Cancelled);
        }
        match ack_rx.await {
            Ok(true) => Ok(()),
            // 写失败 → 明确报错，绝不静默跳过（F-01）
            Ok(false) => Err(Error::ShortWrite {
                offset,
                want: len as usize,
                got: 0,
            }),
            Err(_) => Err(Error::Cancelled),
        }
    }

    /// 服务器不支持 Range 时的单流下载（F-04：缓冲固定 256 KB，永不退化为 1 字节）。
    async fn run_single(
        self: &Arc<Self>,
        client: &reqwest::Client,
        url: &Url,
        headers: &[(String, String)],
        probe: &Probe,
    ) -> Result<()> {
        const BUF: usize = 256 * 1024;
        let token = self.token();

        let mut req = client
            .get(url.clone())
            .header(reqwest::header::ACCEPT_ENCODING, "identity");
        for (k, v) in headers {
            req = req.header(k, v);
        }
        let resp = tokio::select! {
            _ = token.cancelled() => return Err(Error::Cancelled),
            r = req.send() => r?,
        };
        if !resp.status().is_success() {
            return Err(Error::Other(format!("HTTP {}", resp.status().as_u16())));
        }

        // 无 Range 时不可能真正续传：若已有部分数据，只能重下
        let writer = ChunkWriter::create(self.part_path(), probe.total_len)?;
        let mut stream = resp.bytes_stream();
        let mut offset = 0u64;
        self.inner.lock().connections = 1;

        loop {
            let piece = tokio::select! {
                _ = token.cancelled() => break,
                p = stream.next() => p,
            };
            let Some(piece) = piece else { break };
            let piece = piece?;
            let mut buf = &piece[..];
            while !buf.is_empty() {
                let n = buf.len().min(BUF);
                writer.write_at(offset, &buf[..n])?;
                buf = &buf[n..];
                offset += n as u64;
                self.commit((offset - n as u64, offset));
            }
            self.save_meta(false)?;
        }
        writer.sync_all()?;
        {
            let mut inner = self.inner.lock();
            inner.connections = 0;
            inner.total = Some(offset);
        }
        Ok(())
    }

    fn finish_file(self: &Arc<Self>, total: Option<u64>) -> Result<()> {
        let part = self.part_path();
        let final_path = self.final_path();

        // 校验：已完成区间必须覆盖全长
        if let Some(t) = total {
            let covered = self.done.lock().covered_len();
            if covered != t {
                return Err(Error::LengthMismatch {
                    expected: t,
                    actual: covered,
                });
            }
        }

        // 落盘后再改名，避免"看起来完成但数据还在页缓存"
        {
            let w = ChunkWriter::open_existing(&part, total)?;
            w.sync_all()?;
            if let Some(t) = total {
                w.verify_len(t)?;
            }
        }

        // 目标已存在则顺延编号
        let final_path = unique_path(&final_path);
        std::fs::rename(&part, &final_path)?;
        self.inner.lock().filename = final_path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "download".to_string());
        Ok(())
    }
}

/// 区间租约：Drop 时自动归还未提交部分。
struct RangeLease {
    task: Arc<Task>,
    seg: Segment,
    committed: u64,
    armed: bool,
}

impl RangeLease {
    fn new(task: Arc<Task>, seg: Segment) -> Self {
        Self {
            task,
            seg,
            committed: 0,
            armed: true,
        }
    }

    fn advance(&mut self, n: u64) {
        self.committed += n;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RangeLease {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let done_end = self.seg.0 + self.committed;
        if done_end < self.seg.1 {
            self.task.put_segment((done_end, self.seg.1));
        }
    }
}

struct WriteCmd {
    offset: u64,
    data: Bytes,
    seg: Segment,
    ack: oneshot::Sender<bool>,
}

fn build_bitfield(done: Vec<Segment>, total: Option<u64>, piece_len: u64) -> Vec<u8> {
    let Some(total) = total else { return Vec::new() };
    if total == 0 || piece_len == 0 {
        return Vec::new();
    }
    let n = total.div_ceil(piece_len) as usize;
    let mut bits = vec![0u8; n.div_ceil(8)];
    for (a, b) in done {
        let start = (a / piece_len) as usize;
        let end = ((b.saturating_sub(1)) / piece_len) as usize;
        for i in start..=end.min(n.saturating_sub(1)) {
            bits[i / 8] |= 1 << (7 - (i % 8));
        }
    }
    bits
}

/// 第 `i` 个分片是否已下载完。
///
/// 与 [`build_bitfield`] 严格对称：高位在前，第 `i` 片是
/// `byte[i/8] & (1 << (7 - i%8))`。方向写反会让进度图左右镜像。
pub fn piece_done(bitfield: &[u8], i: usize) -> bool {
    match bitfield.get(i / 8) {
        Some(b) => b & (1 << (7 - (i % 8))) != 0,
        None => false,
    }
}

/// 把分片位图还原成**连续的已完成区间**（字节，半开区间 `[start, end)`）。
///
/// 相邻分片会被合并，于是「前 32 片完成」显示成一段 `0–32 MB`，而不是罗列 32 行。
pub fn done_segments(bitfield: &[u8], num_pieces: u64, piece_len: u64) -> Vec<Segment> {
    if piece_len == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut start: Option<u64> = None;
    for i in 0..num_pieces as usize {
        match (piece_done(bitfield, i), start) {
            (true, None) => start = Some(i as u64),
            (false, Some(s)) => {
                out.push((s * piece_len, i as u64 * piece_len));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push((s * piece_len, num_pieces * piece_len));
    }
    out
}

/// 未完成区间：已完成区间在 `[0, num_pieces*piece_len)` 上的补集。
pub fn pending_segments(bitfield: &[u8], num_pieces: u64, piece_len: u64) -> Vec<Segment> {
    if piece_len == 0 {
        return Vec::new();
    }
    let total = num_pieces * piece_len;
    let mut out = Vec::new();
    let mut cur = 0u64;
    for (a, b) in done_segments(bitfield, num_pieces, piece_len) {
        if a > cur {
            out.push((cur, a));
        }
        cur = cur.max(b);
    }
    if cur < total {
        out.push((cur, total));
    }
    out
}

/// 把区间列表格式化成可读文本，超出 `limit` 段时折叠。
///
/// 例：`0 B–32.00 MB · 48.00–49.00 MB（共 3 段）`
pub fn fmt_segments(ranges: &[Segment], limit: usize) -> String {
    if ranges.is_empty() {
        return "无".to_string();
    }
    let shown: Vec<String> = ranges
        .iter()
        .take(limit)
        .map(|&(a, b)| format!("{}–{}", size_str(a), size_str(b)))
        .collect();
    let mut s = shown.join(" · ");
    if ranges.len() > limit {
        s.push_str(&format!(" … 另 {} 段", ranges.len() - limit));
    } else if ranges.len() > 1 {
        s.push_str(&format!("（共 {} 段）", ranges.len()));
    }
    s
}

fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".into());
    let ext = path
        .extension()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    for i in 1..=9999 {
        let name = if ext.is_empty() {
            format!("{stem} ({i})")
        } else {
            format!("{stem} ({i}).{ext}")
        };
        let cand = path.with_file_name(name);
        if !cand.exists() {
            return cand;
        }
    }
    path.to_path_buf()
}

fn error_code_of(e: &Error) -> i32 {
    // 参考 aria2 错误码，便于客户端识别
    match e {
        Error::NoRangeSupport => 1,
        Error::RemoteChanged { .. } => 8,
        Error::LengthMismatch { .. } => 8,
        Error::ShortWrite { .. } => 8,
        Error::Io(_) | Error::Http(_) => 1,
        Error::Cancelled => 7,
        _ => 1,
    }
}

/// 文件名清洗（供 RPC 层复用）。
pub fn clean_out_name(raw: &str) -> String {
    sanitize_filename(raw)
}

/// 便于日志：把若干秒数格式化为 `HH:MM:SS`。
pub fn eta_str(remaining: u64, speed: u64) -> String {
    if speed == 0 {
        return "--:--:--".into();
    }
    let secs = remaining / speed;
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// 滑动窗口速度采样器。
///
/// 不能只算「相邻两次采样的增量」：进度是成批落盘的（一次 commit 可能就是 10MB），
/// 而采样每秒一次，于是多数采样秒的增量为 0，速度被硬置 0、只在 commit 那一秒飙高，
/// UI 上表现为速度几乎恒为 0、ETA 永远是「—」。
/// 改成保留最近 `SPEED_WINDOW` 内的样本、用窗口首尾差值计算，速度就平滑了。
#[derive(Default)]
struct SpeedMeter {
    /// (采样时刻, 当时的累计完成字节)，按时间递增。
    samples: VecDeque<(Instant, u64)>,
}

impl SpeedMeter {
    /// 记录一次采样并返回当前速度（字节/秒）；样本不足时返回 None。
    fn sample(&mut self, bytes: u64) -> Option<u64> {
        let now = Instant::now();
        self.samples.push_back((now, bytes));

        // 丢弃窗口外的旧样本，但至少留 2 个，否则算不出差值
        while self.samples.len() > 2 {
            let oldest = self.samples.front().unwrap().0;
            if now.duration_since(oldest) > SPEED_WINDOW {
                self.samples.pop_front();
            } else {
                break;
            }
        }
        if self.samples.len() < 2 {
            return None;
        }

        let (t0, b0) = *self.samples.front().unwrap();
        let dt = now.duration_since(t0).as_secs_f64();
        if dt < 0.2 {
            return None;
        }
        // 单调保护：续传/重置导致字节回退时按 0 计，避免算出负速度
        let delta = bytes.saturating_sub(b0);
        Some((delta as f64 / dt) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(total: Option<u64>) -> Arc<Task> {
        let (tx, _rx) = broadcast::channel(16);
        let t = Arc::new(Task::new(
            1,
            vec!["https://example.com/f".into()],
            TaskOptions::default(),
            tx,
        ));
        t.inner.lock().total = total;
        t
    }

    #[test]
    fn commit_only_after_write() {
        let t = task(Some(100));
        *t.remaining.lock() = PendingRanges::full(100);
        assert_eq!(t.completed(), 0);

        t.commit((0, 40));
        assert_eq!(t.completed(), 40);
        assert_eq!(t.snapshot().completed, 40);

        t.commit((40, 100));
        assert_eq!(t.completed(), 100);
    }

    #[test]
    fn lease_returns_uncommitted_on_drop() {
        let t = task(Some(1000));
        *t.remaining.lock() = PendingRanges::full(1000);

        let seg = t.take_segment(400).unwrap();
        assert_eq!(seg, (0, 400));
        assert_eq!(t.remaining.lock().remaining(), 600);

        {
            let mut lease = RangeLease::new(t.clone(), seg);
            lease.advance(100);
            // 模拟异常：没有 disarm
        }
        // 剩余 [100,400) 应被归还
        assert_eq!(t.remaining.lock().segments(), vec![(100, 1000)]);
    }

    #[test]
    fn lease_disarmed_does_not_return() {
        let t = task(Some(1000));
        *t.remaining.lock() = PendingRanges::full(1000);
        let seg = t.take_segment(400).unwrap();
        {
            let mut lease = RangeLease::new(t.clone(), seg);
            lease.advance(400);
            lease.disarm();
        }
        assert_eq!(t.remaining.lock().remaining(), 600);
    }

    #[test]
    fn bitfield_marks_completed_pieces() {
        // 4 MB 文件，piece = 1 MB → 4 个 piece；完成 [0,1MB) 与 [2MB,3MB)
        let bf = build_bitfield(
            vec![(0, PIECE_LEN), (2 * PIECE_LEN, 3 * PIECE_LEN)],
            Some(4 * PIECE_LEN),
            PIECE_LEN,
        );
        assert_eq!(bf.len(), 1);
        assert_eq!(bf[0], 0b1010_0000);
    }

    #[test]
    fn piece_done_matches_bitfield_layout() {
        let bf = vec![0b1010_0000u8];
        assert!(piece_done(&bf, 0));
        assert!(!piece_done(&bf, 1));
        assert!(piece_done(&bf, 2));
        assert!(!piece_done(&bf, 3));
        // 越界返回 false 而不是 panic
        assert!(!piece_done(&bf, 100));
    }

    #[test]
    fn done_segments_merges_adjacent_pieces() {
        let bf = build_bitfield(
            vec![(0, PIECE_LEN), (2 * PIECE_LEN, 3 * PIECE_LEN)],
            Some(4 * PIECE_LEN),
            PIECE_LEN,
        );
        assert_eq!(
            done_segments(&bf, 4, PIECE_LEN),
            vec![(0, PIECE_LEN), (2 * PIECE_LEN, 3 * PIECE_LEN)]
        );
        assert_eq!(
            pending_segments(&bf, 4, PIECE_LEN),
            vec![(PIECE_LEN, 2 * PIECE_LEN), (3 * PIECE_LEN, 4 * PIECE_LEN)]
        );
    }

    #[test]
    fn segments_degenerate_cases_are_safe() {
        assert!(done_segments(&[], 0, PIECE_LEN).is_empty());
        assert!(pending_segments(&[], 0, PIECE_LEN).is_empty());
        // piece_len 为 0 时直接返回，避免除零 / 溢出
        assert!(done_segments(&[0xff], 8, 0).is_empty());
        assert_eq!(fmt_segments(&[], 4), "无");
    }

    #[test]
    fn fmt_segments_folds_overflow() {
        let many: Vec<Segment> = (0..6)
            .map(|i| (i * PIECE_LEN, (i + 1) * PIECE_LEN))
            .collect();
        let s = fmt_segments(&many, 4);
        assert!(s.contains("另 2 段"), "实际输出: {s}");
    }

    #[test]
    fn eta_formats() {
        assert_eq!(eta_str(0, 0), "--:--:--");
        assert_eq!(eta_str(3600, 1), "01:00:00");
        assert_eq!(eta_str(90, 1), "00:01:30");
    }

    #[test]
    fn unique_path_appends_index() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.zip");
        std::fs::write(&p, b"x").unwrap();
        let u = unique_path(&p);
        assert_eq!(u.file_name().unwrap(), "a (1).zip");
    }
}
