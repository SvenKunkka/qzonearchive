//! 数据源适配层：为每种数据源提供独立同步状态、游标、限流与错误记录。
//!
//! 已接入的数据源：
//! - `feeds`（互动列表，mobile.qzone.qq.com/get_feeds）——归档主源，状态由
//!   archive.rs 归档循环维护；
//! - `album_list`（相册列表，fcg_list_album_v3）——见 sources/albums.rs。
//!
//! 需在真实账号验证后接入（Phase 2 之后）：shuoshuo（本人说说）、
//! album_photos:<albumId>（相册内照片）、guestbook（留言板）、feed_detail（动态详情）。
//! 未验证的接口一律不在此层调用。

pub mod albums;
pub mod shuoshuo;

use rusqlite::{params, Connection};
use serde::Serialize;

use crate::util::now;

pub const SOURCE_FEEDS: &str = "feeds";
pub const SOURCE_ALBUM_LIST: &str = "album_list";
pub const SOURCE_SHUOSHUO: &str = "shuoshuo";

/// 每数据源同步状态（对应 source_states 表）。
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceStateInfo {
    pub source: String,
    pub cursor: String,
    pub status: String,
    pub last_sync_at: Option<i64>,
    pub next_sync_at: Option<i64>,
    pub last_error: Option<String>,
    pub total_fetched: u64,
    pub total_saved: u64,
    pub updated_at: i64,
}

/// 创建或更新某数据源的同步状态。
pub fn upsert_source_state(
    conn: &Connection,
    owner_uin: &str,
    source: &str,
    status: &str,
    cursor: Option<&str>,
    fetched: u64,
    saved: u64,
    error: Option<&str>,
) -> Result<(), String> {
    let cursor_value = cursor.unwrap_or("").to_owned();
    let now_value = now();
    conn.execute(
        "INSERT INTO source_states
         (owner_uin,source,cursor,status,last_sync_at,last_error,total_fetched,total_saved,updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
         ON CONFLICT(owner_uin,source) DO UPDATE SET
          cursor=excluded.cursor,status=excluded.status,
          last_sync_at=COALESCE(excluded.last_sync_at,source_states.last_sync_at),
          last_error=excluded.last_error,
          total_fetched=source_states.total_fetched+excluded.total_fetched,
          total_saved=source_states.total_saved+excluded.total_saved,
          updated_at=excluded.updated_at",
        params![
            owner_uin,
            source,
            cursor_value,
            status,
            now_value,
            error.unwrap_or(""),
            fetched,
            saved,
            now_value,
        ],
    )
    .map_err(|error| format!("保存数据源同步状态失败：{error}"))?;
    Ok(())
}

/// 仅更新状态字段（游标与计数不动），用于开始/结束/失败时的状态迁移。
pub fn set_source_status(
    conn: &Connection,
    owner_uin: &str,
    source: &str,
    status: &str,
    error: Option<&str>,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO source_states(owner_uin,source,status,last_error,updated_at)
         VALUES (?1,?2,?3,?4,?5)
         ON CONFLICT(owner_uin,source) DO UPDATE SET
          status=excluded.status,last_error=excluded.last_error,updated_at=excluded.updated_at",
        params![owner_uin, source, status, error.unwrap_or(""), now()],
    )
    .map_err(|error| format!("更新数据源状态失败：{error}"))?;
    Ok(())
}

/// 读取某数据源的同步状态（不存在时返回 None）。
#[allow(dead_code)]
pub fn load_source_state(
    conn: &Connection,
    owner_uin: &str,
    source: &str,
) -> Result<Option<SourceStateInfo>, String> {
    match conn.query_row(
        "SELECT source,cursor,status,last_sync_at,next_sync_at,last_error,total_fetched,total_saved,updated_at
         FROM source_states WHERE owner_uin=?1 AND source=?2",
        params![owner_uin, source],
        |row| {
            Ok(SourceStateInfo {
                source: row.get(0)?,
                cursor: row.get(1)?,
                status: row.get(2)?,
                last_sync_at: row.get(3)?,
                next_sync_at: row.get(4)?,
                last_error: row.get(5)?,
                total_fetched: row.get::<_, i64>(6)?.max(0) as u64,
                total_saved: row.get::<_, i64>(7)?.max(0) as u64,
                updated_at: row.get(8)?,
            })
        },
    ) {
        Ok(state) => Ok(Some(state)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(format!("读取数据源同步状态失败：{error}")),
    }
}

/// 列出当前账号全部数据源的同步状态。
pub fn list_source_states(
    conn: &Connection,
    owner_uin: &str,
) -> Result<Vec<SourceStateInfo>, String> {
    let mut statement = conn
        .prepare(
            "SELECT source,cursor,status,last_sync_at,next_sync_at,last_error,total_fetched,total_saved,updated_at
             FROM source_states WHERE owner_uin=?1 ORDER BY source",
        )
        .map_err(|error| format!("读取数据源列表失败：{error}"))?;
    let rows = statement
        .query_map(params![owner_uin], |row| {
            Ok(SourceStateInfo {
                source: row.get(0)?,
                cursor: row.get(1)?,
                status: row.get(2)?,
                last_sync_at: row.get(3)?,
                next_sync_at: row.get(4)?,
                last_error: row.get(5)?,
                total_fetched: row.get::<_, i64>(6)?.max(0) as u64,
                total_saved: row.get::<_, i64>(7)?.max(0) as u64,
                updated_at: row.get(8)?,
            })
        })
        .map_err(|error| format!("查询数据源列表失败：{error}"))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("解析数据源列表失败：{error}"))
}

/// 重置某数据源的同步状态（清空游标与统计，用于强制全量重新同步）。
pub fn reset_source_state(conn: &Connection, owner_uin: &str, source: &str) -> Result<(), String> {
    conn.execute(
        "DELETE FROM source_states WHERE owner_uin=?1 AND source=?2",
        params![owner_uin, source],
    )
    .map_err(|error| format!("重置数据源状态失败：{error}"))?;
    Ok(())
}

/// 通用请求频率保护：每 10 分钟最多 300 次接口页请求。
/// 返回 Some(retry_at) 表示已达上限，应暂停到该时间。
pub(crate) fn reserve_request_slot(
    conn: &Connection,
    owner_uin: &str,
) -> Result<Option<i64>, String> {
    const WINDOW_SECONDS: i64 = 10 * 60;
    const PAGE_LIMIT: i64 = 300;
    let current = now();
    let state = conn.query_row(
        "SELECT window_started_at,requested_pages FROM archive_rate_limits WHERE owner_uin=?1",
        params![owner_uin],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
    );
    match state {
        Ok((started_at, pages)) if current - started_at < WINDOW_SECONDS && pages >= PAGE_LIMIT => {
            Ok(Some(started_at + WINDOW_SECONDS))
        }
        Ok((started_at, _)) if current - started_at >= WINDOW_SECONDS => {
            conn.execute(
                "UPDATE archive_rate_limits SET window_started_at=?2,requested_pages=1 WHERE owner_uin=?1",
                params![owner_uin, current],
            )
            .map_err(|error| format!("重置归档频率窗口失败：{error}"))?;
            Ok(None)
        }
        Ok(_) => {
            conn.execute(
                "UPDATE archive_rate_limits SET requested_pages=requested_pages+1 WHERE owner_uin=?1",
                params![owner_uin],
            )
            .map_err(|error| format!("记录归档请求频率失败：{error}"))?;
            Ok(None)
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            conn.execute(
                "INSERT INTO archive_rate_limits(owner_uin,window_started_at,requested_pages) VALUES (?1,?2,1)",
                params![owner_uin, current],
            )
            .map_err(|error| format!("创建归档频率窗口失败：{error}"))?;
            Ok(None)
        }
        Err(error) => Err(format!("读取归档请求频率失败：{error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(name: &str) -> rusqlite::Connection {
        let path = std::env::temp_dir().join(format!(
            "qza-sources-{}-{}-{}.sqlite3",
            name,
            std::process::id(),
            crate::util::now_millis()
        ));
        let _ = std::fs::remove_file(&path);
        crate::db::open_database_at(&path).unwrap()
    }

    #[test]
    fn upserts_and_reads_source_state() {
        let connection = temp_db("state");
        upsert_source_state(
            &connection,
            "10001",
            SOURCE_FEEDS,
            "running",
            Some("cursor-1"),
            10,
            8,
            None,
        )
        .unwrap();
        let state = load_source_state(&connection, "10001", SOURCE_FEEDS)
            .unwrap()
            .unwrap();
        assert_eq!(state.status, "running");
        assert_eq!(state.cursor, "cursor-1");
        assert_eq!(state.total_fetched, 10);
        assert_eq!(state.total_saved, 8);

        // 第二次写入应累加计数并更新游标
        upsert_source_state(
            &connection,
            "10001",
            SOURCE_FEEDS,
            "running",
            Some("cursor-2"),
            5,
            5,
            None,
        )
        .unwrap();
        let state = load_source_state(&connection, "10001", SOURCE_FEEDS)
            .unwrap()
            .unwrap();
        assert_eq!(state.cursor, "cursor-2");
        assert_eq!(state.total_fetched, 15);
        assert_eq!(state.total_saved, 13);
    }

    #[test]
    fn sets_status_and_error_without_touching_counts() {
        let connection = temp_db("status");
        upsert_source_state(
            &connection,
            "10001",
            SOURCE_ALBUM_LIST,
            "running",
            Some("x"),
            3,
            2,
            None,
        )
        .unwrap();
        set_source_status(
            &connection,
            "10001",
            SOURCE_ALBUM_LIST,
            "error",
            Some("网络错误"),
        )
        .unwrap();
        let state = load_source_state(&connection, "10001", SOURCE_ALBUM_LIST)
            .unwrap()
            .unwrap();
        assert_eq!(state.status, "error");
        assert_eq!(state.last_error.as_deref(), Some("网络错误"));
        assert_eq!(state.total_fetched, 3);
        assert_eq!(state.total_saved, 2);
        assert_eq!(state.cursor, "x");
    }

    #[test]
    fn lists_and_resets_source_states() {
        let connection = temp_db("list");
        upsert_source_state(
            &connection,
            "10001",
            SOURCE_FEEDS,
            "completed",
            Some("c"),
            1,
            1,
            None,
        )
        .unwrap();
        upsert_source_state(
            &connection,
            "10001",
            SOURCE_ALBUM_LIST,
            "idle",
            Some(""),
            0,
            0,
            None,
        )
        .unwrap();
        let states = list_source_states(&connection, "10001").unwrap();
        assert_eq!(states.len(), 2);
        reset_source_state(&connection, "10001", SOURCE_FEEDS).unwrap();
        let states = list_source_states(&connection, "10001").unwrap();
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].source, SOURCE_ALBUM_LIST);
    }

    #[test]
    fn rate_limiter_enforces_window() {
        let connection = temp_db("ratelimit");
        // 前 299 次放行
        for _ in 0..299 {
            assert!(reserve_request_slot(&connection, "10001")
                .unwrap()
                .is_none());
        }
        // 第 300 次放行
        assert!(reserve_request_slot(&connection, "10001")
            .unwrap()
            .is_none());
        // 第 301 次触发上限
        let retry_at = reserve_request_slot(&connection, "10001").unwrap();
        assert!(retry_at.is_some(), "达到上限应返回重试时间");
    }
}

/// Tauri 命令：列出当前账号全部数据源的同步状态。
#[tauri::command]
pub async fn list_source_states_command(
    app: tauri::AppHandle,
    login: tauri::State<'_, crate::qlogin::QLoginState>,
) -> Result<Vec<SourceStateInfo>, String> {
    let owner_uin = login.qzone_auth().await?.uin;
    tauri::async_runtime::spawn_blocking(move || {
        let connection = crate::db::open_database(&app)?;
        list_source_states(&connection, &owner_uin)
    })
    .await
    .map_err(|error| format!("数据源状态查询任务异常退出：{error}"))?
}

/// Tauri 命令：重置某数据源的同步状态（强制全量重新同步）。
#[tauri::command]
pub async fn reset_source_state_command(
    app: tauri::AppHandle,
    login: tauri::State<'_, crate::qlogin::QLoginState>,
    source: String,
) -> Result<(), String> {
    if source != SOURCE_FEEDS && source != SOURCE_ALBUM_LIST {
        return Err("未知的数据源".into());
    }
    let owner_uin = login.qzone_auth().await?.uin;
    tauri::async_runtime::spawn_blocking(move || {
        let connection = crate::db::open_database(&app)?;
        reset_source_state(&connection, &owner_uin, &source)
    })
    .await
    .map_err(|error| format!("数据源重置任务异常退出：{error}"))?
}
