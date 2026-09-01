//! 本人说说数据源（emotion_cgi_msglist_v6）。
//!
//! 接口形态基于社区长期运行的开源项目交叉验证（非编造）：
//! - Ritori2022/qq-space-export（qzone_api_export.js）
//! - ll0v0ll/GetQzonehistory / Gentlesprite/GetQzonehistory（GetAllMomentsUtil.py）
//!
//! URL: https://user.qzone.qq.com/proxy/domain/taotao.qq.com/cgi-bin/emotion_cgi_msglist_v6
//! 参数: uin, ftype=0, sort=0, pos(偏移分页), num(每页), replynum=100, g_tk,
//!       callback=_preloadCallback, code_version=1, format=jsonp, need_private_comment=1
//! 返回: JSONP `_preloadCallback({code, total, msglist[]})`
//! msglist 元素字段（GetQzonehistory 提取路径）：content / name / created_time /
//!       pic[].url1 / video[].url1 / commentlist[](content, createTime2, name, uin)
//!
//! 说明：该接口能拉取当前账号全部未删除的可见说说（含从未被互动过的），
//! 是互动列表之外最大的历史资料增量。唯一 ID 字段名（tid/cellid 候选）与
//! uin 字段在真实响应验证时确认，未确认前使用指纹候选键。

use rusqlite::params;
use serde::Serialize;
use serde_json::Value;

use crate::db;
use crate::qlogin::QLoginState;
use crate::sources::{
    reserve_request_slot, set_source_status, upsert_source_state, SOURCE_SHUOSHUO,
};
use crate::util::{int_at, now, text_at};

const SHUOSHUO_LIST_URL: &str =
    "https://user.qzone.qq.com/proxy/domain/taotao.qq.com/cgi-bin/emotion_cgi_msglist_v6";
const PAGE_SIZE: i64 = 30;
/// 安全上限：单次同步最多 5000 页（15 万条），防止异常死循环。
const MAX_PAGES: i64 = 5000;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShuoshuoSyncResult {
    pub total: u64,
    pub fetched: u64,
    pub saved: u64,
    pub existing: u64,
}

/// 拉取一页说说。返回 (msglist, total)。
async fn fetch_shuoshuo_page(
    login: &QLoginState,
    pos: i64,
    num: i64,
) -> Result<(Vec<Value>, i64), String> {
    let auth = login.qzone_auth().await?;
    let response = login
        .client()
        .get(SHUOSHUO_LIST_URL)
        .header(reqwest::header::ACCEPT, "*/*")
        .header(
            reqwest::header::REFERER,
            format!("https://user.qzone.qq.com/{}/main", auth.uin),
        )
        .header(reqwest::header::USER_AGENT, &auth.user_agent)
        .header(reqwest::header::COOKIE, &auth.cookie_header)
        .query(&[
            ("uin", auth.uin.as_str()),
            ("ftype", "0"),
            ("sort", "0"),
            ("pos", &pos.to_string()),
            ("num", &num.to_string()),
            ("replynum", "100"),
            ("g_tk", &auth.g_tk.to_string()),
            ("callback", "_preloadCallback"),
            ("code_version", "1"),
            ("format", "jsonp"),
            ("need_private_comment", "1"),
        ])
        .send()
        .await
        .map_err(|error| format!("获取说说列表失败：{error}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| format!("读取说说列表响应失败：{error}"))?;
    if !status.is_success() {
        return Err(format!("获取说说列表失败：HTTP {status}"));
    }
    let value = crate::qzone::parse_qzone_json(&text)
        .map_err(|error| format!("解析说说列表响应失败：{error}"))?;
    let code = value.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code != 0 {
        let message = value
            .get("message")
            .or_else(|| value.get("msg"))
            .and_then(Value::as_str)
            .unwrap_or("未知错误");
        return Err(format!("说说列表接口返回错误 {code}：{message}"));
    }
    let items = value
        .get("msglist")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let total = value.get("total").and_then(Value::as_i64).unwrap_or(0);
    Ok((items, total))
}

/// 说说唯一键：优先平台 ID（多候选字段，容忍缺失），否则用指纹。
fn shuoshuo_key(item: &Value) -> String {
    if let Some(id) = text_at(item, "/tid")
        .or_else(|| text_at(item, "/cellid"))
        .or_else(|| text_at(item, "/cell_id/cellid"))
        .filter(|value| !value.is_empty())
    {
        return format!("shuoshuo:{id}");
    }
    let content = text_at(item, "/content").unwrap_or_default();
    let created = int_at(item, "/created_time").unwrap_or(0);
    let author = text_at(item, "/uin").unwrap_or_else(|| "unknown".to_owned());
    format!(
        "shuoshuo-fallback:{author}:{created}:{}",
        crate::util::sha256_hex(content.trim().as_bytes())
    )
}

/// 把 msglist 元素归一化为 archive_dynamics 可用的图片/视频 JSON
/// （复用现有媒体管道：/picdata/pic[].photourl[].url 与 /videourl）。
fn normalized_media_json(item: &Value) -> (Option<String>, Option<String>) {
    let pictures = item.get("pic").and_then(Value::as_array).map(|pics| {
        let normalized: Vec<Value> = pics
            .iter()
            .filter_map(|pic| {
                let url = text_at(pic, "/url1")?;
                Some(serde_json::json!({
                    "photourl": [{ "url": url }]
                }))
            })
            .collect();
        serde_json::json!({ "picdata": { "pic": normalized } })
    });
    let video = item
        .get("video")
        .and_then(Value::as_array)
        .and_then(|videos| videos.iter().find_map(|video| text_at(video, "/url1")))
        .map(|url| serde_json::json!({ "videourl": url }));
    (
        pictures.map(|value| value.to_string()),
        video.map(|value| value.to_string()),
    )
}

/// 保存一条说说到统一模型（archive_dynamics + users + comments）。
fn save_shuoshuo(
    transaction: &rusqlite::Transaction<'_>,
    owner_uin: &str,
    item: &Value,
    raw_response_id: Option<i64>,
) -> Result<Option<i64>, String> {
    let key = shuoshuo_key(item);
    let created_time = int_at(item, "/created_time").unwrap_or(0);
    let author_uin = text_at(item, "/uin").unwrap_or_else(|| owner_uin.to_owned());
    let author_name = text_at(item, "/name");
    let content = text_at(item, "/content").unwrap_or_default();
    let (pictures_json, video_json) = normalized_media_json(item);
    let fingerprint = crate::model::content_fingerprint(
        Some(&author_uin),
        created_time,
        Some(&content),
        pictures_json.as_deref(),
        video_json.as_deref(),
    );
    let now_value = now();
    let existed: i64 = transaction
        .query_row(
            "SELECT COUNT(*) FROM archive_dynamics WHERE owner_uin=?1 AND cell_id=?2",
            params![owner_uin, key],
            |row| row.get(0),
        )
        .map_err(|error| format!("检查说说去重失败：{error}"))?;
    let id: i64 = transaction.query_row(
        "INSERT INTO archive_dynamics
         (owner_uin,cell_id,published_at,content,author_uin,author_name,category,pictures_json,video_json,
          raw_original_json,archived_at,source,content_fingerprint,remote_status,first_seen_at,last_seen_at)
         VALUES (?1,?2,?3,?4,?5,?6,'self',?7,?8,?9,?10,'shuoshuo',?11,'active',?12,?12)
         ON CONFLICT(owner_uin,cell_id) DO UPDATE SET
          published_at=excluded.published_at,content=excluded.content,author_uin=excluded.author_uin,
          author_name=excluded.author_name,pictures_json=COALESCE(excluded.pictures_json,archive_dynamics.pictures_json),
          video_json=COALESCE(excluded.video_json,archive_dynamics.video_json),
          raw_original_json=excluded.raw_original_json,archived_at=excluded.archived_at,
          content_fingerprint=excluded.content_fingerprint,last_seen_at=excluded.last_seen_at
         RETURNING id",
        params![
            owner_uin,
            key,
            created_time,
            content,
            author_uin,
            author_name,
            pictures_json,
            video_json,
            item.to_string(),
            now_value,
            fingerprint,
            now_value,
        ],
        |row| row.get(0),
    )
    .map_err(|error| format!("保存说说失败：{error}"))?;
    if existed == 0 {
        transaction
            .execute(
                "INSERT INTO dynamic_sources(dynamic_id,owner_uin,source,raw_response_id,matched_by,fetched_at)
                 VALUES (?1,?2,'shuoshuo',?3,'new',?4)
                 ON CONFLICT(dynamic_id,source) DO UPDATE SET
                  raw_response_id=excluded.raw_response_id,matched_by=excluded.matched_by,fetched_at=excluded.fetched_at",
                params![id, owner_uin, raw_response_id, now_value],
            )
            .map_err(|error| format!("记录说说来源失败：{error}"))?;
    }
    if !author_uin.is_empty() {
        transaction
            .execute(
                "INSERT INTO users(uin,nickname,first_seen_at,last_seen_at)
                 VALUES (?1,?2,?3,?3)
                 ON CONFLICT(uin) DO UPDATE SET
                  nickname=COALESCE(NULLIF(excluded.nickname,''),users.nickname),last_seen_at=excluded.last_seen_at",
                params![author_uin, author_name, now_value],
            )
            .map_err(|error| format!("保存说说作者失败：{error}"))?;
    }
    // 评论（字段路径来自 GetQzonehistory：content / createTime2 / name / uin）
    if let Some(comments) = item.get("commentlist").and_then(Value::as_array) {
        for comment in comments {
            let comment_uin = text_at(comment, "/uin");
            let comment_name = text_at(comment, "/name");
            let comment_content = text_at(comment, "/content").unwrap_or_default();
            let comment_time = int_at(comment, "/createTime2")
                .or_else(|| int_at(comment, "/create_time"))
                .unwrap_or(0);
            if let Some(uin) = comment_uin.as_deref() {
                transaction
                    .execute(
                        "INSERT INTO users(uin,nickname,first_seen_at,last_seen_at)
                         VALUES (?1,?2,?3,?3)
                         ON CONFLICT(uin) DO UPDATE SET
                          nickname=COALESCE(NULLIF(excluded.nickname,''),users.nickname),last_seen_at=excluded.last_seen_at",
                        params![uin, comment_name, now_value],
                    )
                    .map_err(|error| format!("保存评论用户失败：{error}"))?;
            }
            // commentlist 无唯一评论 ID，按（作者, 内容, 时间）去重
            let duplicate: i64 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM comments
                     WHERE owner_uin=?1 AND dynamic_id=?2 AND uin=?3 AND content=?4 AND created_at=?5",
                    params![owner_uin, id, comment_uin, comment_content, comment_time],
                    |row| row.get(0),
                )
                .map_err(|error| format!("检查说说评论去重失败：{error}"))?;
            if duplicate == 0 {
                transaction
                    .execute(
                        "INSERT INTO comments(owner_uin,dynamic_id,uin,nickname,content,created_at,raw_json)
                         VALUES (?1,?2,?3,?4,?5,?6,?7)",
                        params![
                            owner_uin,
                            id,
                            comment_uin,
                            comment_name,
                            comment_content,
                            comment_time,
                            comment.to_string(),
                        ],
                    )
                    .map_err(|error| format!("保存说说评论失败：{error}"))?;
            }
        }
    }
    Ok(Some(id))
}

/// Tauri 命令：全量同步本人说说（分页，限流，间隔保护）。
#[tauri::command]
pub async fn sync_shuoshuo_command(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
    interval_ms: Option<u64>,
) -> Result<ShuoshuoSyncResult, String> {
    let interval_ms = interval_ms.unwrap_or(1_500).clamp(800, 10_000);
    let owner_uin = login.qzone_auth().await?.uin;
    {
        let connection = db::open_database(&app)?;
        if let Some(retry_at) = reserve_request_slot(&connection, &owner_uin)? {
            return Err(format!("ARCHIVE_RATE_LIMIT:{retry_at}"));
        }
        set_source_status(&connection, &owner_uin, SOURCE_SHUOSHUO, "running", None)?;
    }
    let (first_items, total) = fetch_shuoshuo_page(&login, 0, PAGE_SIZE).await?;
    let result =
        sync_shuoshuo_pages(&app, &login, &owner_uin, total, first_items, interval_ms).await?;
    let connection = db::open_database(&app)?;
    upsert_source_state(
        &connection,
        &owner_uin,
        SOURCE_SHUOSHUO,
        "completed",
        Some(""),
        result.fetched,
        result.saved,
        None,
    )?;
    Ok(result)
}

async fn sync_shuoshuo_pages(
    app: &tauri::AppHandle,
    login: &QLoginState,
    owner_uin: &str,
    total: i64,
    first_items: Vec<Value>,
    interval_ms: u64,
) -> Result<ShuoshuoSyncResult, String> {
    let mut result = ShuoshuoSyncResult {
        total: total.max(0) as u64,
        fetched: 0,
        saved: 0,
        existing: 0,
    };
    let mut pos = 0_i64;
    let mut items = first_items;
    loop {
        if items.is_empty() {
            break;
        }
        result.fetched += items.len() as u64;
        // DB 连接（非 Send）在独立块内使用，避免跨 await 存活
        {
            let mut connection = db::open_database(app)?;
            let transaction = connection
                .transaction()
                .map_err(|error| format!("开始说说同步事务失败：{error}"))?;
            let mut saved = 0;
            let mut existing = 0;
            for item in &items {
                let key = shuoshuo_key(item);
                let already: i64 = transaction
                    .query_row(
                        "SELECT COUNT(*) FROM archive_dynamics WHERE owner_uin=?1 AND cell_id=?2",
                        params![owner_uin, key],
                        |row| row.get(0),
                    )
                    .map_err(|error| format!("检查说说去重失败：{error}"))?;
                if already > 0 {
                    existing += 1;
                }
                save_shuoshuo(&transaction, owner_uin, item, None)?;
                saved += 1;
            }
            transaction
                .commit()
                .map_err(|error| format!("提交说说同步事务失败：{error}"))?;
            result.saved += saved;
            result.existing += existing;
        }
        pos += PAGE_SIZE;
        if pos >= total || result.fetched >= total.max(0) as u64 || pos / PAGE_SIZE >= MAX_PAGES {
            break;
        }
        {
            let connection = db::open_database(app)?;
            if let Some(retry_at) = reserve_request_slot(&connection, owner_uin)? {
                return Err(format!("ARCHIVE_RATE_LIMIT:{retry_at}"));
            }
        }
        match fetch_shuoshuo_page(login, pos, PAGE_SIZE).await {
            Ok((next_items, _)) => items = next_items,
            Err(error) => {
                // 单页失败不中断整体（记入源状态错误，返回部分结果）
                let connection = db::open_database(app)?;
                set_source_status(
                    &connection,
                    owner_uin,
                    SOURCE_SHUOSHUO,
                    "error",
                    Some(&error),
                )?;
                return Err(error);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_msg() -> Value {
        json!({
            "content": "今天天气不错",
            "name": "我自己",
            "created_time": 1700000000,
            "uin": "10001",
            "tid": "tid-123",
            "pic": [{ "url1": "https://a.example/p1.jpg" }, { "url1": "https://a.example/p2.jpg" }],
            "commentlist": [
                { "content": "好看", "createTime2": 1700000100, "name": "朋友A", "uin": "10002" }
            ]
        })
    }

    #[test]
    fn prefers_platform_id_then_falls_back_to_fingerprint() {
        let item = sample_msg();
        assert_eq!(shuoshuo_key(&item), "shuoshuo:tid-123");
        let mut no_id = json!({ "content": "abc", "created_time": 100 });
        assert!(shuoshuo_key(&no_id).starts_with("shuoshuo-fallback:"));
        no_id["content"] = json!("abd");
        assert_ne!(
            shuoshuo_key(&no_id),
            shuoshuo_key(&json!({"content":"abc","created_time":100}))
        );
    }

    #[test]
    fn normalizes_pictures_and_video_for_media_pipeline() {
        let item = json!({
            "pic": [{ "url1": "https://a.example/1.jpg" }],
            "video": [{ "url1": "https://v.example/v.mp4" }]
        });
        let (pictures, video) = normalized_media_json(&item);
        let pics = serde_json::from_str::<Value>(&pictures.unwrap()).unwrap();
        assert_eq!(
            pics["picdata"]["pic"][0]["photourl"][0]["url"],
            "https://a.example/1.jpg"
        );
        let v = serde_json::from_str::<Value>(&video.unwrap()).unwrap();
        assert_eq!(v["videourl"], "https://v.example/v.mp4");
    }

    #[test]
    fn saves_shuoshuo_with_dedup_and_comments() {
        let path = std::env::temp_dir().join(format!(
            "qza-shuoshuo-{}-{}.sqlite3",
            std::process::id(),
            crate::util::now_millis()
        ));
        let _ = std::fs::remove_file(&path);
        let mut connection = crate::db::open_database_at(&path).unwrap();
        let item = sample_msg();
        let saved_id;
        {
            let transaction = connection.transaction().unwrap();
            saved_id = save_shuoshuo(&transaction, "10001", &item, None)
                .unwrap()
                .unwrap();
            transaction.commit().unwrap();
            assert!(saved_id > 0);
        }
        {
            let transaction = connection.transaction().unwrap();
            let id2 = save_shuoshuo(&transaction, "10001", &item, None)
                .unwrap()
                .unwrap();
            transaction.commit().unwrap();
            assert_eq!(id2, saved_id, "同一条说说应合并到同一行");
        }
        let dynamic_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM archive_dynamics", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(dynamic_count, 1);
        let source: String = connection
            .query_row("SELECT source FROM archive_dynamics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(source, "shuoshuo");
        let comment_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM comments", [], |row| row.get(0))
            .unwrap();
        assert_eq!(comment_count, 1);
        // 媒体管道可拾取图片
        crate::media::sync_media_items_db(&connection, "10001").unwrap();
        let media_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM media_items", [], |row| row.get(0))
            .unwrap();
        assert!(
            media_count >= 2,
            "图片应被登记为媒体条目，实际 {media_count}"
        );
        let _ = std::fs::remove_file(&path);
    }
}
