//! 相册列表数据源（fcg_list_album_v3）。
//!
//! 解析采用与前端 RecycleBinView 一致的容忍式字段候选（字段名来自现有代码对
//! 真实响应的观察）：数组可能在 `album`/`albums`/`albumList` 下，ID 候选
//! `albumid`/`albumId`/`id`，名称候选 `name`/`albumname`/`title`，
//! 照片数候选 `num`/`photoNum`/`total`。
//!
//! 同步语义：
//! - 本次响应中出现的相册 upsert 到 albums 表并标记 last_seen；
//! - 库中已有但本次未出现的相册标记 remote_status='remote_deleted'（不删除本地数据）；
//! - 每源同步状态写入 source_states('album_list')。

use rusqlite::{params, Connection};
use serde::Serialize;
use serde_json::Value;

use crate::db;
use crate::qlogin::QLoginState;
use crate::sources::{
    reserve_request_slot, set_source_status, upsert_source_state, SOURCE_ALBUM_LIST,
};
use crate::util::now;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedAlbum {
    pub album_id: String,
    pub name: String,
    pub photo_count: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AlbumSyncResult {
    pub listed: u64,
    pub saved: u64,
    pub remote_marked: u64,
    pub total: u64,
}

fn as_object(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    value.as_object()
}

fn first_array<'a>(root: &'a Value, keys: &[&str]) -> Option<&'a Vec<Value>> {
    let data = root.get("data").and_then(as_object);
    for key in keys {
        if let Some(array) = root.get(key).and_then(Value::as_array) {
            return Some(array);
        }
        if let Some(array) = data
            .and_then(|data| data.get(*key))
            .and_then(Value::as_array)
        {
            return Some(array);
        }
    }
    None
}

fn first_string<'a>(item: &'a Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        let value = item.get(key);
        if let Some(text) = value
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            return Some(text.to_owned());
        }
        if let Some(number) = value.and_then(Value::as_i64) {
            return Some(number.to_string());
        }
    }
    None
}

/// 容忍式解析相册列表（与前端 mapAlbums 字段候选一致）。
pub fn parse_album_list(response: &Value) -> Vec<ParsedAlbum> {
    let Some(items) = first_array(response, &["album", "albums", "albumList"]) else {
        return vec![];
    };
    items
        .iter()
        .filter_map(|item| {
            let album_id = first_string(item, &["albumid", "albumId", "id"])?;
            if album_id.is_empty() {
                return None;
            }
            let name = first_string(item, &["name", "albumname", "title"])
                .unwrap_or_else(|| "未命名相册".to_owned());
            let photo_count = first_string(item, &["num", "photoNum", "total"])
                .and_then(|value| value.parse::<i64>().ok());
            Some(ParsedAlbum {
                album_id,
                name,
                photo_count,
            })
        })
        .collect()
}

/// 同步相册列表：请求 → 解析 → 入库 → 标记消失 → 更新状态。
pub fn sync_album_list(
    conn: &mut Connection,
    owner_uin: &str,
    response: &Value,
) -> Result<AlbumSyncResult, String> {
    let albums = parse_album_list(response);
    let now_value = now();
    let transaction = conn
        .transaction()
        .map_err(|error| format!("开始相册同步事务失败：{error}"))?;
    let mut saved = 0_u64;
    let mut seen = std::collections::HashSet::new();
    for album in &albums {
        seen.insert(album.album_id.clone());
        let changed = transaction
            .execute(
                "INSERT INTO albums(owner_uin,album_id,name,photo_count,raw_json,remote_status,last_seen_at)
                 VALUES (?1,?2,?3,?4,?5,'active',?6)
                 ON CONFLICT(owner_uin,album_id) DO UPDATE SET
                  name=excluded.name,photo_count=excluded.photo_count,raw_json=excluded.raw_json,
                  remote_status=CASE WHEN albums.remote_status='remote_deleted' THEN 'active' ELSE albums.remote_status END,
                  last_seen_at=excluded.last_seen_at",
                params![
                    owner_uin,
                    album.album_id,
                    album.name,
                    album.photo_count,
                    response.to_string(),
                    now_value,
                ],
            )
            .map_err(|error| format!("保存相册失败：{error}"))?;
        saved += changed as u64;
    }
    // 库中已有但本次未出现 → 标记远端消失（不删除本地数据）
    let remote_marked = transaction
        .execute(
            "UPDATE albums SET remote_status='remote_deleted'
             WHERE owner_uin=?1 AND remote_status='active' AND album_id NOT IN (SELECT value FROM json_each(?2))",
            params![owner_uin, serde_json::json!(seen.iter().collect::<Vec<_>>()).to_string()],
        )
        .map_err(|error| format!("标记远端消失相册失败：{error}"))?;
    let total: i64 = transaction
        .query_row(
            "SELECT COUNT(*) FROM albums WHERE owner_uin=?1",
            params![owner_uin],
            |row| row.get(0),
        )
        .map_err(|error| format!("统计相册失败：{error}"))?;
    transaction
        .commit()
        .map_err(|error| format!("提交相册同步事务失败：{error}"))?;
    Ok(AlbumSyncResult {
        listed: albums.len() as u64,
        saved,
        remote_marked: remote_marked.max(0) as u64,
        total: total.max(0) as u64,
    })
}

/// Tauri 命令：同步当前账号的相册列表。
#[tauri::command]
pub async fn sync_album_list_command(
    app: tauri::AppHandle,
    login: tauri::State<'_, QLoginState>,
) -> Result<AlbumSyncResult, String> {
    let owner_uin = login.qzone_auth().await?.uin;
    {
        let connection = db::open_database(&app)?;
        if let Some(retry_at) = reserve_request_slot(&connection, &owner_uin)? {
            return Err(format!("ARCHIVE_RATE_LIMIT:{retry_at}"));
        }
        set_source_status(&connection, &owner_uin, SOURCE_ALBUM_LIST, "running", None)?;
    }
    let response = match crate::qzone::fetch_album_list(&login).await {
        Ok(response) => response,
        Err(error) => {
            let connection = db::open_database(&app)?;
            set_source_status(
                &connection,
                &owner_uin,
                SOURCE_ALBUM_LIST,
                "error",
                Some(&error),
            )?;
            return Err(error);
        }
    };
    let result = {
        let mut connection = db::open_database(&app)?;
        let result = sync_album_list(&mut connection, &owner_uin, &response)?;
        upsert_source_state(
            &connection,
            &owner_uin,
            SOURCE_ALBUM_LIST,
            "completed",
            Some(""),
            result.listed,
            result.saved,
            None,
        )?;
        result
    };
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_album_list_with_tolerant_fields() {
        let response = json!({
            "code": 0,
            "data": {
                "albumList": [
                    {"albumId": "a1", "name": "相册一", "num": 5},
                    {"id": "a2", "albumname": "相册二"}
                ]
            }
        });
        let albums = parse_album_list(&response);
        assert_eq!(albums.len(), 2);
        assert_eq!(albums[0].album_id, "a1");
        assert_eq!(albums[0].name, "相册一");
        assert_eq!(albums[0].photo_count, Some(5));
        assert_eq!(albums[1].album_id, "a2");
        assert_eq!(albums[1].photo_count, None);
    }

    #[test]
    fn accepts_album_array_variants() {
        for key in ["album", "albums"] {
            let response = json!({ "data": { key: [{ "albumid": "x1", "title": "T" }] } });
            let albums = parse_album_list(&response);
            assert_eq!(albums.len(), 1, "{key} 应可解析");
            assert_eq!(albums[0].name, "T");
        }
        let response = json!({ "albumList": [{ "albumId": "x2" }] });
        assert_eq!(parse_album_list(&response).len(), 1);
    }

    #[test]
    fn sync_marks_vanished_albums_without_deleting() {
        let path = std::env::temp_dir().join(format!(
            "qza-albums-sync-{}-{}.sqlite3",
            std::process::id(),
            crate::util::now_millis()
        ));
        let _ = std::fs::remove_file(&path);
        let mut connection = crate::db::open_database_at(&path).unwrap();
        // 预置两个相册
        connection
            .execute(
                "INSERT INTO albums(owner_uin,album_id,name,remote_status,last_seen_at) VALUES ('10001','a1','旧相册','active',1)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO albums(owner_uin,album_id,name,remote_status,last_seen_at) VALUES ('10001','a2','保留相册','active',1)",
                [],
            )
            .unwrap();
        // 本次响应只包含 a2
        let response =
            json!({ "data": { "albumList": [{ "albumId": "a2", "name": "保留相册" }] } });
        let result = sync_album_list(&mut connection, "10001", &response).unwrap();
        assert_eq!(result.listed, 1);
        assert_eq!(result.remote_marked, 1, "a1 应被标记为远端消失");
        let statuses: Vec<(String, String)> = {
            let mut statement = connection
                .prepare("SELECT album_id,remote_status FROM albums WHERE owner_uin='10001' ORDER BY album_id")
                .unwrap();
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .unwrap();
            rows.filter_map(Result::ok).collect()
        };
        assert_eq!(statuses.len(), 2, "本地数据不应被删除");
        assert!(statuses
            .iter()
            .any(|(id, status)| id == "a1" && status == "remote_deleted"));
        assert!(statuses
            .iter()
            .any(|(id, status)| id == "a2" && status == "active"));
        // 再次同步包含 a1 → 恢复 active
        let response = json!({ "data": { "albumList": [{ "albumId": "a1", "name": "旧相册" }, { "albumId": "a2" }] } });
        sync_album_list(&mut connection, "10001", &response).unwrap();
        let a1_status: String = connection
            .query_row(
                "SELECT remote_status FROM albums WHERE album_id='a1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(a1_status, "active");
        let _ = std::fs::remove_file(&path);
    }
}
