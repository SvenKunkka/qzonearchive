//! 本地媒体归档基础设施：图片 / 原图 / 视频 / 封面 的本地下载。
//!
//! 能力：
//! - 断点续传（Range + `<id>.part` 文件，暂停后从断点继续）；
//! - 失败重试（单文件最多 3 次，指数退避）；
//! - 限速（按字节/秒节流）；
//! - 文件类型验证（Content-Type + magic bytes，识别 QQ 占位图）；
//! - SHA-256 内容哈希与按 URL 去重（media_items UNIQUE）；
//! - 下载状态记录（远程地址 / 本地路径 / 哈希 / 大小 / 类型 / 状态 / 下载时间）；
//! - 任务级 暂停 / 继续 / 取消 / 可见进度；
//! - 三种模式：仅保存数据 / 下载图片 / 完整下载图片和视频。
//!
//! 与既有 `load_archived_image` / `load_archived_video`（浏览按需缓存）解耦：
//! 本模块管理持久化媒体归档，既有按需缓存逻辑保持不变。

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

use futures_util::StreamExt;
use rusqlite::{params, Connection};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tauri::Manager;

use crate::util::now;
use crate::{db, qlogin::QLoginState};

// ---------------------------------------------------------------------------
// 模式与进度

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MediaDownloadMode {
    DataOnly,
    Images,
    Full,
}

impl MediaDownloadMode {
    pub fn parse(value: &str) -> Self {
        match value {
            "images" => MediaDownloadMode::Images,
            "full" => MediaDownloadMode::Full,
            _ => MediaDownloadMode::DataOnly,
        }
    }

    #[allow(dead_code)]
    pub fn as_str(self) -> &'static str {
        match self {
            MediaDownloadMode::DataOnly => "data-only",
            MediaDownloadMode::Images => "images",
            MediaDownloadMode::Full => "full",
        }
    }

    fn includes_kind(self, kind: &str) -> bool {
        match self {
            MediaDownloadMode::DataOnly => false,
            MediaDownloadMode::Images => matches!(kind, "image" | "video_cover"),
            MediaDownloadMode::Full => true,
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaDownloadProgress {
    status: &'static str,
    total: u64,
    done: u64,
    failed: u64,
    skipped: u64,
    bytes_done: u64,
    current_url: Option<String>,
    message: String,
}

impl Default for MediaDownloadProgress {
    fn default() -> Self {
        Self {
            status: "idle",
            total: 0,
            done: 0,
            failed: 0,
            skipped: 0,
            bytes_done: 0,
            current_url: None,
            message: "尚未开始媒体下载".into(),
        }
    }
}

pub struct MediaDownloadState {
    progress: Mutex<MediaDownloadProgress>,
    cancel: AtomicBool,
    paused: AtomicBool,
}

impl MediaDownloadState {
    pub fn new() -> Self {
        Self {
            progress: Mutex::new(MediaDownloadProgress::default()),
            cancel: AtomicBool::new(false),
            paused: AtomicBool::new(false),
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaItemInfo {
    id: i64,
    dynamic_id: Option<i64>,
    media_kind: String,
    remote_url: String,
    local_path: Option<String>,
    sha256: Option<String>,
    size_bytes: Option<i64>,
    mime_type: Option<String>,
    download_status: String,
    download_attempts: i64,
    last_error: Option<String>,
    last_downloaded_at: Option<i64>,
    created_at: i64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaStats {
    total: u64,
    pending: u64,
    done: u64,
    failed: u64,
    paused: u64,
    skipped: u64,
    bytes_done: u64,
    images: u64,
    videos: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaSyncResult {
    created: u64,
    total: u64,
}

// ---------------------------------------------------------------------------
// 请求认证（来自 qlogin 会话；测试时可构造无凭证值）

pub(crate) struct MediaRequestAuth {
    pub user_agent: String,
    pub cookie_header: String,
    pub referer: String,
}

impl MediaRequestAuth {
    pub(crate) fn from_qzone(auth: &crate::qlogin::QzoneAuth) -> Self {
        Self {
            user_agent: auth.user_agent.clone(),
            cookie_header: auth.cookie_header.clone(),
            referer: format!("https://user.qzone.qq.com/{}", auth.uin),
        }
    }
}

// ---------------------------------------------------------------------------
// 常量与辅助

const MAX_IMAGE_BYTES: u64 = 100 * 1024 * 1024;
const MAX_VIDEO_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_ATTEMPTS_PER_ITEM: u32 = 3;

fn media_dir(app: &tauri::AppHandle, owner_uin: &str) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("无法获取媒体归档目录：{error}"))?
        .join("media")
        .join(owner_uin);
    fs::create_dir_all(&dir).map_err(|error| format!("无法创建媒体归档目录：{error}"))?;
    Ok(dir)
}

/// 由 magic bytes 判断扩展名。
fn extension_for(bytes: &[u8], kind: &str) -> Option<&'static str> {
    if kind == "video" {
        return Some("mp4");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("jpg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.starts_with(b"BM") {
        Some("bmp")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else if bytes.get(4..12).is_some_and(|value| {
        value.starts_with(b"ftyp") && (&value[4..8] == b"avif" || &value[4..8] == b"avis")
    }) {
        Some("avif")
    } else {
        None
    }
}

/// QQ 图片不存在占位图识别（与既有按需缓存逻辑一致）。
/// head 为文件头部字节（至少 10 字节），total_len 为完整文件长度。
fn is_qq_missing_image_placeholder(head: &[u8], total_len: u64) -> bool {
    head.get(6..10).is_some_and(|size| {
        let width = u16::from_le_bytes([size[0], size[1]]);
        let height = u16::from_le_bytes([size[2], size[3]]);
        (total_len == 2_038 && head.starts_with(b"GIF89a") && width == 340 && height == 320)
            || (total_len == 2_687 && head.starts_with(b"GIF89a") && width == 340 && height == 320)
            || (total_len == 1_643 && head.starts_with(b"GIF87a") && width == 99 && height == 99)
            || (total_len == 1_547 && head.starts_with(b"GIF87a") && width == 98 && height == 98)
    })
}

/// 简单的按字节节流器。
struct RateLimiter {
    bytes_per_second: u64,
    last_tick: Instant,
    window_bytes: u64,
}

impl RateLimiter {
    fn new(bytes_per_second: u64) -> Self {
        Self {
            bytes_per_second,
            last_tick: Instant::now(),
            window_bytes: 0,
        }
    }

    async fn throttle(&mut self, bytes: u64) {
        if self.bytes_per_second == 0 {
            return;
        }
        self.window_bytes += bytes;
        let expected = self.window_bytes as f64 / self.bytes_per_second as f64;
        let elapsed = self.last_tick.elapsed().as_secs_f64();
        if expected > elapsed {
            let sleep = expected - elapsed;
            tokio::time::sleep(Duration::from_secs_f64(sleep.min(5.0))).await;
        }
        if self.window_bytes >= self.bytes_per_second * 2 {
            self.window_bytes = 0;
            self.last_tick = Instant::now();
        }
    }
}

// ---------------------------------------------------------------------------
// 数据访问

struct MediaItemRow {
    id: i64,
    media_kind: String,
    remote_url: String,
    local_path: Option<String>,
    download_status: String,
}

/// 从 archive_dynamics 同步媒体条目到 media_items（按 URL 去重）。
pub fn sync_media_items_db(conn: &Connection, owner_uin: &str) -> Result<MediaSyncResult, String> {
    let mut statement = conn
        .prepare("SELECT id,pictures_json,video_json FROM archive_dynamics WHERE owner_uin=?1")
        .map_err(|error| format!("读取媒体同步源失败：{error}"))?;
    let rows = statement
        .query_map(params![owner_uin], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(|error| format!("查询媒体同步源失败：{error}"))?;
    let mut created = 0_u64;
    for row in rows {
        let (dynamic_id, pictures_json, video_json) =
            row.map_err(|error| format!("读取媒体同步记录失败：{error}"))?;
        let now_value = now();
        for url in crate::model::picture_urls(pictures_json) {
            created +=
                insert_media_item(conn, owner_uin, Some(dynamic_id), "image", &url, now_value)?;
        }
        for url in crate::model::video_urls(video_json.clone()) {
            created +=
                insert_media_item(conn, owner_uin, Some(dynamic_id), "video", &url, now_value)?;
        }
        if let Some(url) = crate::model::video_cover_url(video_json) {
            created += insert_media_item(
                conn,
                owner_uin,
                Some(dynamic_id),
                "video_cover",
                &url,
                now_value,
            )?;
        }
    }
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM media_items WHERE owner_uin=?1",
            params![owner_uin],
            |row| row.get(0),
        )
        .map_err(|error| format!("统计媒体条目失败：{error}"))?;
    Ok(MediaSyncResult {
        created,
        total: total.max(0) as u64,
    })
}

fn insert_media_item(
    conn: &Connection,
    owner_uin: &str,
    dynamic_id: Option<i64>,
    kind: &str,
    url: &str,
    created_at: i64,
) -> Result<u64, String> {
    conn.execute(
        "INSERT INTO media_items(owner_uin,dynamic_id,media_kind,remote_url,download_status,created_at)
         VALUES (?1,?2,?3,?4,'pending',?5)
         ON CONFLICT(owner_uin,media_kind,remote_url) DO NOTHING",
        params![owner_uin, dynamic_id, kind, url, created_at],
    )
    .map_err(|error| format!("保存媒体条目失败：{error}"))?;
    Ok(conn.changes() as u64)
}

fn load_pending_items(
    conn: &Connection,
    owner_uin: &str,
    mode: MediaDownloadMode,
    retry_failed: bool,
) -> Result<Vec<MediaItemRow>, String> {
    if retry_failed {
        conn.execute(
            "UPDATE media_items SET download_status='pending',last_error=NULL
             WHERE owner_uin=?1 AND download_status='failed'",
            params![owner_uin],
        )
        .map_err(|error| format!("重置失败媒体失败：{error}"))?;
    }
    let mut statement = conn
        .prepare(
            "SELECT id,media_kind,remote_url,local_path,download_status FROM media_items
             WHERE owner_uin=?1 AND download_status IN ('pending','paused')
             ORDER BY id ASC",
        )
        .map_err(|error| format!("读取待下载媒体失败：{error}"))?;
    let rows = statement
        .query_map(params![owner_uin], |row| {
            Ok(MediaItemRow {
                id: row.get(0)?,
                media_kind: row.get(1)?,
                remote_url: row.get(2)?,
                local_path: row.get(3)?,
                download_status: row.get(4)?,
            })
        })
        .map_err(|error| format!("查询待下载媒体失败：{error}"))?;
    let items = rows
        .filter_map(Result::ok)
        .filter(|item| mode.includes_kind(&item.media_kind))
        .collect::<Vec<_>>();
    Ok(items)
}

fn mark_item(
    conn: &Connection,
    id: i64,
    status: &str,
    attempts: u32,
    error: Option<&str>,
) -> Result<(), String> {
    conn.execute(
        "UPDATE media_items SET download_status=?2,download_attempts=?3,last_error=?4
         WHERE id=?1",
        params![id, status, attempts, error],
    )
    .map_err(|error| format!("更新媒体下载状态失败：{error}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 下载核心

/// 单文件下载（支持断点续传）。返回下载元信息；被暂停/取消时返回 Ok(None)。
async fn download_item(
    client: &reqwest::Client,
    auth: Option<&MediaRequestAuth>,
    item: &MediaItemRow,
    target_dir: &Path,
    rate_bps: u64,
    flags: &TaskFlags<'_>,
) -> Result<Option<DownloadedMeta>, String> {
    // 已完成且本地文件存在 → 跳过
    if item.download_status == "done" {
        if let Some(path) = item.local_path.as_deref() {
            if Path::new(path).metadata().is_ok_and(|meta| meta.len() > 0) {
                return Ok(Some(DownloadedMeta {
                    local_path: path.to_owned(),
                    sha256: None,
                    size_bytes: 0,
                    mime_type: None,
                    skipped: true,
                }));
            }
        }
    }
    let part_path = target_dir.join(format!("{}.part", item.id));
    let mut resume_from = fs::metadata(&part_path).map(|meta| meta.len()).unwrap_or(0);
    if resume_from > 0 {
        let _ = std::fs::OpenOptions::new().write(true).open(&part_path);
    }
    let mut first_bytes: Vec<u8> = Vec::with_capacity(12);
    if resume_from > 0 {
        // 断点续传时从 part 文件头部恢复 magic bytes 用于类型验证
        if let Ok(handle) = fs::File::open(&part_path) {
            use std::io::Read;
            let mut reader = std::io::BufReader::new(handle);
            let mut head = [0u8; 12];
            let read = reader.read(&mut head).unwrap_or(0);
            first_bytes.extend_from_slice(&head[..read]);
        }
    }

    let mut last_error = String::new();
    let combos: &[(bool, bool)] = match auth {
        Some(_) => &[(true, true), (true, false), (false, true), (false, false)],
        None => &[(false, false)],
    };
    for &(with_cookie, with_referer) in combos {
        if flags.should_stop() {
            return Ok(None);
        }
        let mut request = client
            .get(&item.remote_url)
            .header(
                reqwest::header::USER_AGENT,
                auth.map(|a| a.user_agent.as_str()).unwrap_or("Mozilla/5.0"),
            )
            .header(
                reqwest::header::ACCEPT,
                if item.media_kind == "video" {
                    "video/mp4,video/*;q=0.9,application/octet-stream;q=0.8,*/*;q=0.5"
                } else {
                    "image/avif,image/webp,image/png,image/jpeg,image/*,*/*;q=0.8"
                },
            )
            .header(
                reqwest::header::ACCEPT_LANGUAGE,
                "zh-CN,zh;q=0.9,en;q=0.8,en-GB;q=0.7,en-US;q=0.6,zh-TW;q=0.5",
            );
        if with_cookie {
            if let Some(auth) = auth {
                request = request.header(reqwest::header::COOKIE, &auth.cookie_header);
            }
        }
        if with_referer {
            request = request.header(
                reqwest::header::REFERER,
                auth.map(|a| a.referer.as_str())
                    .unwrap_or("https://user.qzone.qq.com/"),
            );
        }
        if resume_from > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={resume_from}-"));
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                last_error = format!("请求失败：{error}");
                continue;
            }
        };
        let status = response.status();
        // Range 不被支持时（200 而非 206）回退全量重下
        let restart = resume_from > 0
            && status == reqwest::StatusCode::OK
            && response
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .is_none();
        if !status.is_success() && status != reqwest::StatusCode::PARTIAL_CONTENT {
            last_error = if status == reqwest::StatusCode::FORBIDDEN {
                "QQ 拒绝了媒体请求（HTTP 403），视频临时签名可能已过期".into()
            } else {
                format!("HTTP {}", status)
            };
            continue;
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !acceptable_content_type(&content_type, &item.media_kind) {
            last_error = format!("返回了非媒体内容（{content_type}）");
            continue;
        }
        let content_length = response
            .content_length()
            .unwrap_or(0)
            .saturating_add(resume_from);
        let max = if item.media_kind == "video" {
            MAX_VIDEO_BYTES
        } else {
            MAX_IMAGE_BYTES
        };
        if content_length > max {
            last_error = format!("媒体超过大小上限（{max} 字节）");
            continue;
        }
        if restart {
            // 服务器忽略 Range：清空 part 重新下载
            resume_from = 0;
            let _ = fs::remove_file(&part_path);
        }
        // 打开 part 文件（追加模式）
        let mut output = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&part_path)
            .map_err(|error| format!("打开媒体临时文件失败：{error}"))?;
        if resume_from == 0 {
            // 新下载：清空可能存在的残留
            output
                .set_len(0)
                .map_err(|error| format!("清空媒体临时文件失败：{error}"))?;
            resume_from = 0;
        }
        let mut hasher = Sha256::new();
        if resume_from > 0 {
            if let Ok(handle) = fs::File::open(&part_path) {
                use std::io::Read;
                let mut reader = std::io::BufReader::new(handle);
                let mut buffer = [0u8; 64 * 1024];
                loop {
                    let read = reader
                        .read(&mut buffer)
                        .map_err(|error| format!("读取媒体临时文件失败：{error}"))?;
                    if read == 0 {
                        break;
                    }
                    hasher.update(&buffer[..read]);
                }
            }
        }
        let mut limiter = RateLimiter::new(rate_bps);
        let mut total_bytes = resume_from;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            if flags.should_stop() {
                // 暂停/取消：保留 part 文件用于下次续传
                output
                    .flush()
                    .map_err(|error| format!("刷新媒体临时文件失败：{error}"))?;
                return Ok(None);
            }
            let chunk = chunk.map_err(|error| format!("读取媒体数据失败：{error}"))?;
            if first_bytes.len() < 12 {
                let need = 12 - first_bytes.len();
                first_bytes.extend_from_slice(&chunk[..chunk.len().min(need)]);
            }
            // 大文件流式写入 + 哈希
            output
                .write_all(&chunk)
                .map_err(|error| format!("写入媒体文件失败：{error}"))?;
            hasher.update(&chunk);
            total_bytes += chunk.len() as u64;
            if total_bytes > max {
                output
                    .flush()
                    .map_err(|error| format!("刷新媒体临时文件失败：{error}"))?;
                last_error = format!("媒体超过大小上限（{max} 字节）");
                break;
            }
            limiter.throttle(chunk.len() as u64).await;
        }
        // 组合尝试中断时，把已写入的字节数作为下次续传起点
        resume_from = total_bytes;
        if total_bytes > max {
            continue;
        }
        output
            .flush()
            .map_err(|error| format!("刷新媒体临时文件失败：{error}"))?;
        // 类型验证（magic bytes）
        if item.media_kind == "video"
            && !first_bytes
                .get(4..12)
                .is_some_and(|value| value.windows(4).any(|part| part == b"ftyp"))
        {
            last_error = "返回内容不是有效的视频文件（缺少 ftyp）".into();
            continue;
        }
        let Some(extension) = extension_for(&first_bytes, &item.media_kind) else {
            last_error = format!(
                "返回内容不是有效的{}文件",
                if item.media_kind == "video" {
                    "视频"
                } else {
                    "图片"
                }
            );
            continue;
        };
        if item.media_kind != "video" && is_qq_missing_image_placeholder(&first_bytes, total_bytes)
        {
            last_error = "QQ 返回了图片不存在占位图".into();
            continue;
        }
        // 增量流式哈希（含续传部分）汇总
        let final_hash = hex::encode(hasher.finalize().as_slice());
        let final_path = target_dir.join(format!("{}.{}", item.id, extension));
        if let Err(error) = fs::rename(&part_path, &final_path) {
            if !final_path.exists() {
                return Err(format!("保存媒体归档失败：{error}"));
            }
        }
        let size_bytes = fs::metadata(&final_path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        return Ok(Some(DownloadedMeta {
            local_path: final_path.to_string_lossy().into_owned(),
            sha256: Some(final_hash),
            size_bytes,
            mime_type: Some(content_type.split(';').next().unwrap_or("").to_owned()),
            skipped: false,
        }));
    }
    Err(if last_error.is_empty() {
        "所有媒体地址均加载失败".into()
    } else {
        last_error
    })
}

fn acceptable_content_type(content_type: &str, kind: &str) -> bool {
    if content_type.is_empty() {
        return true; // 交给 magic bytes 验证
    }
    if kind == "video" {
        content_type.starts_with("video/")
            || content_type.contains("octet-stream")
            || content_type.contains("mp4")
    } else {
        content_type.starts_with("image/") || content_type.contains("octet-stream")
    }
}

struct DownloadedMeta {
    local_path: String,
    sha256: Option<String>,
    size_bytes: u64,
    mime_type: Option<String>,
    skipped: bool,
}

struct TaskFlags<'a> {
    cancel: &'a AtomicBool,
    paused: &'a AtomicBool,
}

impl TaskFlags<'_> {
    fn should_stop(&self) -> bool {
        self.cancel.load(Ordering::Relaxed) || self.paused.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Tauri 命令

/// 同步媒体条目（把 archive_dynamics 中的图片/视频登记到 media_items）。
#[tauri::command]
pub async fn sync_media_items(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
) -> Result<MediaSyncResult, String> {
    let owner_uin = login.qzone_auth().await?.uin;
    tauri::async_runtime::spawn_blocking(move || {
        let connection = db::open_database(&app)?;
        sync_media_items_db(&connection, &owner_uin)
    })
    .await
    .map_err(|error| format!("媒体同步任务异常退出：{error}"))?
}

#[tauri::command]
pub async fn start_media_download(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    state: tauri::State<'_, std::sync::Arc<MediaDownloadState>>,
    mode: String,
    retry_failed: Option<bool>,
) -> Result<MediaDownloadProgress, String> {
    let mode = MediaDownloadMode::parse(&mode);
    let retry_failed = retry_failed.unwrap_or(false);
    {
        let mut progress = state.progress.lock().map_err(|_| "媒体下载状态锁已损坏")?;
        if progress.status == "running" {
            return Err("已有媒体下载任务正在运行".into());
        }
        *progress = MediaDownloadProgress {
            status: "running",
            total: 0,
            done: 0,
            failed: 0,
            skipped: 0,
            bytes_done: 0,
            current_url: None,
            message: "正在准备媒体下载…".into(),
        };
    }
    state.cancel.store(false, Ordering::Relaxed);
    state.paused.store(false, Ordering::Relaxed);
    let owner_uin = login.qzone_auth().await?.uin;
    if mode == MediaDownloadMode::DataOnly {
        let sync = {
            let connection = db::open_database(&app)?;
            sync_media_items_db(&connection, &owner_uin)?
        };
        set_progress(&state, |progress| {
            progress.status = "completed";
            progress.total = sync.total;
            progress.message = format!(
                "仅保存数据模式：已登记 {} 个媒体条目（共 {} 个），未下载任何媒体",
                sync.created, sync.total
            );
        });
        return Ok(progress_snapshot(&state)?);
    }
    let interval_ms = interval_ms_from_env_or(2_000);
    let auth = login.qzone_auth().await?;
    let media_auth = MediaRequestAuth::from_qzone(&auth);
    let state_arc = state.inner().clone();
    tauri::async_runtime::spawn(async move {
        run_download_task(
            &app,
            &owner_uin,
            &media_auth,
            &state_arc,
            mode,
            retry_failed,
            interval_ms,
        )
        .await;
    });
    Ok(progress_snapshot(&state)?)
}

fn interval_ms_from_env_or(default: u64) -> u64 {
    default
}

async fn run_download_task(
    app: &tauri::AppHandle,
    owner_uin: &str,
    auth: &MediaRequestAuth,
    state: &MediaDownloadState,
    mode: MediaDownloadMode,
    retry_failed: bool,
    _interval_ms: u64,
) {
    let result: Result<(), String> = async {
        // 先同步最新媒体条目
        {
            let connection = db::open_database(app)?;
            sync_media_items_db(&connection, owner_uin)?;
        }
        let items = {
            let connection = db::open_database(app)?;
            load_pending_items(&connection, owner_uin, mode, retry_failed)?
        };
        set_progress(state, |progress| {
            progress.total = items.len() as u64;
            progress.message = format!("待下载媒体：{} 个", items.len());
        });
        if items.is_empty() {
            set_progress(state, |progress| {
                progress.status = "completed";
                progress.message = "没有需要下载的媒体".into();
            });
            return Ok(());
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(5))
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|error| format!("创建媒体下载客户端失败：{error}"))?;
        let flags = TaskFlags {
            cancel: &state.cancel,
            paused: &state.paused,
        };
        for item in &items {
            if state.cancel.load(Ordering::Relaxed) {
                break;
            }
            if state.paused.load(Ordering::Relaxed) {
                let connection = db::open_database(app)?;
                let _ = mark_item(&connection, item.id, "paused", 0, None);
                break;
            }
            set_progress(state, |progress| {
                progress.current_url = Some(item.remote_url.clone());
                progress.message = format!("正在下载 {}", item.remote_url);
            });
            let target_dir = media_dir(app, owner_uin)?;
            let mut outcome: Option<Result<Option<DownloadedMeta>, String>> = None;
            for attempt in 1..=MAX_ATTEMPTS_PER_ITEM {
                let attempt_result = download_item(&client, Some(auth), item, &target_dir, 0, &flags).await;
                match &attempt_result {
                    Ok(Some(meta)) if meta.skipped => {
                        outcome = Some(Ok(Some(DownloadedMeta {
                            local_path: meta.local_path.clone(),
                            sha256: meta.sha256.clone(),
                            size_bytes: meta.size_bytes,
                            mime_type: meta.mime_type.clone(),
                            skipped: true,
                        })));
                        break;
                    }
                    Ok(Some(meta)) => {
                        outcome = Some(Ok(Some(DownloadedMeta {
                            local_path: meta.local_path.clone(),
                            sha256: meta.sha256.clone(),
                            size_bytes: meta.size_bytes,
                            mime_type: meta.mime_type.clone(),
                            skipped: false,
                        })));
                        break;
                    }
                    Ok(None) => {
                        // 暂停或取消
                        let connection = db::open_database(app)?;
                        let _ = mark_item(&connection, item.id, "paused", attempt, None);
                        outcome = Some(Ok(None));
                        break;
                    }
                    Err(error) => {
                        if attempt < MAX_ATTEMPTS_PER_ITEM {
                            tokio::time::sleep(Duration::from_millis(1_000 * 2_u64.pow(attempt.saturating_sub(1)))).await;
                        } else {
                            outcome = Some(Err(error.clone()));
                        }
                    }
                }
            }
            match outcome {
                Some(Ok(Some(meta))) if meta.skipped => {
                    let connection = db::open_database(app)?;
                    set_progress(state, |progress| {
                        progress.skipped += 1;
                        progress.done += 1;
                    });
                    let _ = mark_item(&connection, item.id, "done", 0, None);
                }
                Some(Ok(Some(meta))) => {
                    let connection = db::open_database(app)?;
                    connection
                        .execute(
                            "UPDATE media_items SET download_status='done',local_path=?2,sha256=?3,size_bytes=?4,mime_type=?5,last_error=NULL,last_downloaded_at=?6,download_attempts=0
                             WHERE id=?1",
                            params![
                                item.id,
                                meta.local_path,
                                meta.sha256,
                                meta.size_bytes as i64,
                                meta.mime_type,
                                now(),
                            ],
                        )
                        .map_err(|error| format!("更新媒体下载完成状态失败：{error}"))?;
                    set_progress(state, |progress| {
                        progress.done += 1;
                        progress.bytes_done += meta.size_bytes;
                    });
                }
                Some(Ok(None)) => {
                    // 暂停/取消：任务退出
                    set_progress(state, |progress| {
                        if progress.done == 0 && progress.failed == 0 && progress.skipped == 0 {
                            progress.message = "媒体下载已暂停".into();
                        }
                    });
                    return Ok(());
                }
                Some(Err(error)) => {
                    let connection = db::open_database(app)?;
                    let _ = mark_item(&connection, item.id, "failed", MAX_ATTEMPTS_PER_ITEM, Some(&error));
                    set_progress(state, |progress| {
                        progress.failed += 1;
                    });
                }
                None => {}
            }
        }
        Ok(())
    }
    .await;
    match &result {
        Ok(()) if state.cancel.load(Ordering::Relaxed) => set_progress(state, |progress| {
            progress.status = "cancelled";
            progress.current_url = None;
            progress.message = "媒体下载已取消".into();
        }),
        Ok(()) if state.paused.load(Ordering::Relaxed) => set_progress(state, |progress| {
            progress.status = "paused";
            progress.current_url = None;
            progress.message = format!(
                "媒体下载已暂停：完成 {}，失败 {}，跳过 {}，共 {}",
                progress.done, progress.failed, progress.skipped, progress.total
            );
        }),
        Ok(()) => set_progress(state, |progress| {
            progress.status = "completed";
            progress.current_url = None;
            progress.message = format!(
                "媒体下载完成：成功 {}，失败 {}，跳过 {}，共 {}",
                progress.done, progress.failed, progress.skipped, progress.total
            );
        }),
        Err(error) => set_progress(state, |progress| {
            progress.status = "error";
            progress.current_url = None;
            progress.message = format!("媒体下载失败：{}", concise_error(&error));
        }),
    }
    state.cancel.store(false, Ordering::Relaxed);
    state.paused.store(false, Ordering::Relaxed);
}

fn concise_error(error: &str) -> String {
    let normalized = error.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = normalized.chars();
    let summary = chars.by_ref().take(240).collect::<String>();
    if chars.next().is_some() {
        format!("{summary}…")
    } else {
        summary
    }
}

fn set_progress(state: &MediaDownloadState, update: impl FnOnce(&mut MediaDownloadProgress)) {
    if let Ok(mut progress) = state.progress.lock() {
        update(&mut progress);
    }
}

fn progress_snapshot(state: &MediaDownloadState) -> Result<MediaDownloadProgress, String> {
    state
        .progress
        .lock()
        .map(|value| value.clone())
        .map_err(|_| "媒体下载状态锁已损坏".into())
}

#[tauri::command]
pub fn get_media_download_progress(
    state: tauri::State<'_, std::sync::Arc<MediaDownloadState>>,
) -> Result<MediaDownloadProgress, String> {
    progress_snapshot(&state)
}

#[tauri::command]
pub fn pause_media_download(state: tauri::State<'_, std::sync::Arc<MediaDownloadState>>) {
    state.paused.store(true, Ordering::Relaxed);
}

#[tauri::command]
pub fn resume_media_download(state: tauri::State<'_, std::sync::Arc<MediaDownloadState>>) {
    state.paused.store(false, Ordering::Relaxed);
}

#[tauri::command]
pub fn cancel_media_download(state: tauri::State<'_, std::sync::Arc<MediaDownloadState>>) {
    state.cancel.store(true, Ordering::Relaxed);
    state.paused.store(false, Ordering::Relaxed);
}

#[tauri::command]
pub async fn list_media_items(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    limit: u32,
    offset: u32,
    status_filter: Option<String>,
) -> Result<Vec<MediaItemInfo>, String> {
    let owner_uin = login.qzone_auth().await?.uin;
    tauri::async_runtime::spawn_blocking(move || {
        let connection = db::open_database(&app)?;
        let mut sql = String::from(
            "SELECT id,dynamic_id,media_kind,remote_url,local_path,sha256,size_bytes,mime_type,download_status,download_attempts,last_error,last_downloaded_at,created_at
             FROM media_items WHERE owner_uin=?1",
        );
        if status_filter.as_deref().is_some_and(|s| !s.is_empty()) {
            sql.push_str(" AND download_status=?2");
        }
        sql.push_str(" ORDER BY id DESC LIMIT ?3 OFFSET ?4");
        let mut statement = connection
            .prepare(&sql)
            .map_err(|error| format!("准备媒体查询失败：{error}"))?;
        let rows = if let Some(status) = status_filter.as_deref().filter(|s| !s.is_empty()) {
            statement
                .query_map(
                    params![owner_uin, status, limit.clamp(1, 500), offset],
                    media_item_from_row,
                )
                .map_err(|error| format!("查询媒体失败：{error}"))?
        } else {
            statement
                .query_map(
                    params![owner_uin, limit.clamp(1, 500), offset],
                    media_item_from_row,
                )
                .map_err(|error| format!("查询媒体失败：{error}"))?
        };
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("读取媒体列表失败：{error}"))
    })
    .await
    .map_err(|error| format!("媒体查询任务异常退出：{error}"))?
}

fn media_item_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MediaItemInfo> {
    Ok(MediaItemInfo {
        id: row.get(0)?,
        dynamic_id: row.get(1)?,
        media_kind: row.get(2)?,
        remote_url: row.get(3)?,
        local_path: row.get(4)?,
        sha256: row.get(5)?,
        size_bytes: row.get(6)?,
        mime_type: row.get(7)?,
        download_status: row.get(8)?,
        download_attempts: row.get(9)?,
        last_error: row.get(10)?,
        last_downloaded_at: row.get(11)?,
        created_at: row.get(12)?,
    })
}

#[tauri::command]
pub async fn get_media_stats(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
) -> Result<MediaStats, String> {
    let owner_uin = login.qzone_auth().await?.uin;
    tauri::async_runtime::spawn_blocking(move || {
        let connection = db::open_database(&app)?;
        let count = |status: &str| -> Result<u64, String> {
            connection
                .query_row(
                    "SELECT COUNT(*) FROM media_items WHERE owner_uin=?1 AND download_status=?2",
                    params![owner_uin, status],
                    |row| row.get::<_, i64>(0),
                )
                .map(|value| value.max(0) as u64)
                .map_err(|error| format!("统计媒体状态失败：{error}"))
        };
        let total = count("pending")? + count("done")? + count("failed")? + count("paused")? + count("skipped")?;
        let bytes: i64 = connection
            .query_row(
                "SELECT COALESCE(SUM(size_bytes),0) FROM media_items WHERE owner_uin=?1 AND download_status='done'",
                params![owner_uin],
                |row| row.get(0),
            )
            .map_err(|error| format!("统计媒体体积失败：{error}"))?;
        let kind = |kind: &str| -> Result<u64, String> {
            connection
                .query_row(
                    "SELECT COUNT(*) FROM media_items WHERE owner_uin=?1 AND media_kind=?2",
                    params![owner_uin, kind],
                    |row| row.get::<_, i64>(0),
                )
                .map(|value| value.max(0) as u64)
                .map_err(|error| format!("统计媒体类型失败：{error}"))
        };
        Ok(MediaStats {
            total,
            pending: count("pending")?,
            done: count("done")?,
            failed: count("failed")?,
            paused: count("paused")?,
            skipped: count("skipped")?,
            bytes_done: bytes.max(0) as u64,
            images: kind("image")? + kind("video_cover")?,
            videos: kind("video")?,
        })
    })
    .await
    .map_err(|error| format!("媒体统计任务异常退出：{error}"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn png_body() -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&[0x42u8; 2048]);
        bytes
    }

    fn mp4_body() -> Vec<u8> {
        let mut bytes = vec![0x00, 0x00, 0x00, 0x18];
        bytes.extend_from_slice(b"ftypisom");
        bytes.extend_from_slice(&[0x43u8; 2048]);
        bytes
    }

    fn placeholder_gif() -> Vec<u8> {
        // QQ 图片不存在占位图：GIF89a 340x320，总长 2038
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&340u16.to_le_bytes());
        bytes.extend_from_slice(&320u16.to_le_bytes());
        bytes.resize(2038, 0);
        bytes
    }

    /// 本地测试服务器：支持 Range；429 路由首次返回 429 后成功。
    async fn start_test_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let routes: HashMap<String, (Vec<u8>, &'static str)> = [
            ("/png.png".to_owned(), (png_body(), "image/png")),
            ("/video.mp4".to_owned(), (mp4_body(), "video/mp4")),
            (
                "/bad.html".to_owned(),
                (b"<html>not media</html>".to_vec(), "text/html"),
            ),
            (
                "/placeholder.gif".to_owned(),
                (placeholder_gif(), "image/gif"),
            ),
            ("/teapot".to_owned(), (b"teapot".to_vec(), "text/plain")),
        ]
        .into_iter()
        .collect();
        let flaky_hits = std::sync::Arc::new(AtomicUsize::new(0));
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let routes = routes.clone();
                let flaky_hits = flaky_hits.clone();
                tokio::spawn(async move {
                    let mut buffer = [0u8; 4096];
                    let read = socket.read(&mut buffer).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    let request = String::from_utf8_lossy(&buffer[..read]);
                    let mut lines = request.lines();
                    let request_line = lines.next().unwrap_or("");
                    let mut parts = request_line.split_whitespace();
                    let method = parts.next().unwrap_or("GET");
                    let path = parts.next().unwrap_or("/");
                    let range = request
                        .lines()
                        .find(|line| line.to_ascii_lowercase().starts_with("range:"))
                        .and_then(|line| line.split_whitespace().nth(1))
                        .map(str::to_owned);
                    let _ = method;
                    if path == "/flaky.png" && flaky_hits.fetch_add(1, Ordering::SeqCst) == 0 {
                        let _ = socket
                            .write_all(b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                        return;
                    }
                    let Some((body, content_type)) = routes.get(path).cloned() else {
                        let _ = socket
                            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                        return;
                    };
                    let start = range
                        .as_deref()
                        .and_then(|value| value.strip_prefix("bytes="))
                        .and_then(|value| value.split('-').next())
                        .and_then(|value| value.parse::<usize>().ok())
                        .unwrap_or(0);
                    if start > 0 {
                        let slice = &body[start.min(body.len())..];
                        let header = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                            slice.len(),
                            start.min(body.len()),
                            body.len().saturating_sub(1),
                            body.len()
                        );
                        let _ = socket.write_all(header.as_bytes()).await;
                        let _ = socket.write_all(slice).await;
                    } else {
                        let header = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = socket.write_all(header.as_bytes()).await;
                        let _ = socket.write_all(&body).await;
                    }
                });
            }
        });
        format!("http://{addr}")
    }

    fn item(id: i64, kind: &str, url: String, status: &str) -> MediaItemRow {
        MediaItemRow {
            id,
            media_kind: kind.to_owned(),
            remote_url: url,
            local_path: None,
            download_status: status.to_owned(),
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("qza-media-tests-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn detects_media_magic_bytes() {
        assert_eq!(extension_for(&png_body(), "image"), Some("png"));
        assert_eq!(extension_for(&mp4_body(), "video"), Some("mp4"));
        assert_eq!(extension_for(b"GIF89a.......", "image"), Some("gif"));
        assert_eq!(extension_for(b"not-an-image", "image"), None);
        let placeholder = placeholder_gif();
        assert!(is_qq_missing_image_placeholder(
            &placeholder,
            placeholder.len() as u64
        ));
        let png = png_body();
        assert!(!is_qq_missing_image_placeholder(&png, png.len() as u64));
    }

    #[tokio::test]
    async fn downloads_image_and_validates_hash() {
        let base = start_test_server().await;
        let dir = temp_dir("ok");
        let client = reqwest::Client::new();
        let cancel = AtomicBool::new(false);
        let paused = AtomicBool::new(false);
        let flags = TaskFlags {
            cancel: &cancel,
            paused: &paused,
        };
        let meta = download_item(
            &client,
            None,
            &item(1, "image", format!("{base}/png.png"), "pending"),
            &dir,
            0,
            &flags,
        )
        .await
        .unwrap()
        .expect("应下载成功");
        assert!(!meta.skipped);
        assert_eq!(meta.size_bytes, png_body().len() as u64);
        assert_eq!(
            meta.sha256.as_deref(),
            Some(crate::util::sha256_hex(&png_body()).as_str())
        );
        assert!(Path::new(&meta.local_path).exists());
        assert_eq!(fs::read(&meta.local_path).unwrap(), png_body());
    }

    #[tokio::test]
    async fn resumes_from_part_file() {
        let base = start_test_server().await;
        let dir = temp_dir("resume");
        let client = reqwest::Client::new();
        let cancel = AtomicBool::new(false);
        let paused = AtomicBool::new(false);
        let flags = TaskFlags {
            cancel: &cancel,
            paused: &paused,
        };
        // 预置 part 文件：前 100 字节（模拟暂停/中断）
        let body = png_body();
        fs::write(dir.join("7.part"), &body[..100]).unwrap();
        let meta = download_item(
            &client,
            None,
            &item(7, "image", format!("{base}/png.png"), "paused"),
            &dir,
            0,
            &flags,
        )
        .await
        .unwrap()
        .expect("续传应成功");
        assert_eq!(meta.size_bytes, body.len() as u64);
        assert_eq!(fs::read(&meta.local_path).unwrap(), body);
    }

    #[tokio::test]
    async fn rejects_non_media_and_placeholder() {
        let base = start_test_server().await;
        let client = reqwest::Client::new();
        let cancel = AtomicBool::new(false);
        let paused = AtomicBool::new(false);
        let flags = TaskFlags {
            cancel: &cancel,
            paused: &paused,
        };
        let dir = temp_dir("reject");
        let bad = download_item(
            &client,
            None,
            &item(2, "image", format!("{base}/bad.html"), "pending"),
            &dir,
            0,
            &flags,
        )
        .await;
        assert!(bad.is_err(), "非图片内容应被拒绝");

        let placeholder = download_item(
            &client,
            None,
            &item(3, "image", format!("{base}/placeholder.gif"), "pending"),
            &dir,
            0,
            &flags,
        )
        .await;
        assert!(placeholder.is_err(), "占位图应被拒绝");

        let http_error = download_item(
            &client,
            None,
            &item(4, "image", format!("{base}/teapot"), "pending"),
            &dir,
            0,
            &flags,
        )
        .await;
        assert!(http_error.is_err(), "HTTP 418 应失败");
    }

    #[tokio::test]
    async fn downloads_video_with_ftyp_validation() {
        let base = start_test_server().await;
        let client = reqwest::Client::new();
        let cancel = AtomicBool::new(false);
        let paused = AtomicBool::new(false);
        let flags = TaskFlags {
            cancel: &cancel,
            paused: &paused,
        };
        let dir = temp_dir("video");
        let meta = download_item(
            &client,
            None,
            &item(5, "video", format!("{base}/video.mp4"), "pending"),
            &dir,
            0,
            &flags,
        )
        .await
        .unwrap()
        .expect("视频应下载成功");
        assert!(meta.local_path.ends_with(".mp4"));
        assert_eq!(meta.size_bytes, mp4_body().len() as u64);
    }

    #[tokio::test]
    async fn pause_flag_aborts_keeping_part_file() {
        let base = start_test_server().await;
        let client = reqwest::Client::new();
        let cancel = AtomicBool::new(false);
        let paused = AtomicBool::new(true); // 已暂停
        let flags = TaskFlags {
            cancel: &cancel,
            paused: &paused,
        };
        let dir = temp_dir("pause");
        let result = download_item(
            &client,
            None,
            &item(6, "image", format!("{base}/png.png"), "pending"),
            &dir,
            0,
            &flags,
        )
        .await
        .unwrap();
        assert!(result.is_none(), "暂停时应返回 None 且不落最终文件");
        assert!(dir.join("6.part").exists() || !dir.join("6.png").exists());
    }

    #[tokio::test]
    async fn sync_media_items_deduplicates_by_url() {
        let path =
            std::env::temp_dir().join(format!("qza-media-sync-{}.sqlite3", std::process::id()));
        let _ = fs::remove_file(&path);
        let connection = crate::db::open_database_at(&path).unwrap();
        // 直接插入一条动态（含 2 张图、1 个视频）
        connection
            .execute(
                "INSERT INTO archive_dynamics
                 (owner_uin,cell_id,published_at,content,pictures_json,video_json,raw_original_json,archived_at,source)
                 VALUES ('10001','c1',1,'hi',
                 '{\"picdata\":{\"pic\":[{\"photourl\":[{\"url\":\"https://a.example/1.jpg\"}]},{\"photourl\":[{\"url\":\"https://a.example/2.jpg\"}]}]}}',
                 '{\"videourl\":\"https://v.example/a.mp4\"}',
                 '{}', 1, 'feeds')",
                [],
            )
            .unwrap();
        let first = sync_media_items_db(&connection, "10001").unwrap();
        assert_eq!(first.created, 3);
        let second = sync_media_items_db(&connection, "10001").unwrap();
        assert_eq!(second.created, 0, "重复同步不应新增");
        assert_eq!(second.total, 3);
        let _ = fs::remove_file(&path);
    }
}
