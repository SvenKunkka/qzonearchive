//! 版本化数据库迁移框架。
//!
//! - 用 `PRAGMA user_version` 记录 schema 版本（0 = 全新库 / 未迁移旧库）；
//! - 每个 Migration 在一个事务内应用，成功后提升 user_version；
//! - 只增不改：新表独立创建，既有表最多 ADD COLUMN（带默认值）；
//! - 旧库打开时按顺序补跑缺失版本，保证与现状等价且幂等；
//! - 迁移前对数据库做 `PRAGMA quick_check`，损坏则拒绝打开。
//!
//! v1 = 存量 schema 与历史数据迁移（从 archive.rs 原样搬入）；
//! v2 = 统一模型扩展（Raw 层、数据源状态、用户、媒体、互动分表等）。

use std::path::PathBuf;

use rusqlite::Connection;
use tauri::Manager;

use crate::model;

#[allow(dead_code)]
pub const SCHEMA_VERSION: i64 = 3;

pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub apply: fn(&rusqlite::Transaction) -> Result<(), String>,
}

pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "v1_legacy_schema",
        apply: migration_v1,
    },
    Migration {
        version: 2,
        name: "v2_unified_model",
        apply: migration_v2,
    },
    Migration {
        version: 3,
        name: "v3_source_state_columns",
        apply: migration_v3,
    },
];

pub fn database_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("无法获取应用数据目录：{error}"))?;
    std::fs::create_dir_all(&dir).map_err(|error| format!("无法创建应用数据目录：{error}"))?;
    Ok(dir.join("qzone-archive.sqlite3"))
}

/// 打开数据库并执行未完成的迁移（生产入口）。
pub fn open_database(app: &tauri::AppHandle) -> Result<Connection, String> {
    open_database_at(&database_path(app)?)
}

/// 打开指定路径数据库并执行未完成的迁移（测试入口）。
pub fn open_database_at(path: &std::path::Path) -> Result<Connection, String> {
    let mut connection =
        Connection::open(path).map_err(|error| format!("无法打开归档数据库：{error}"))?;
    connection
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
        .map_err(|error| format!("初始化归档数据库失败：{error}"))?;
    migrate(&mut connection)?;
    Ok(connection)
}

/// 执行所有高于当前 user_version 的迁移。
fn migrate(connection: &mut Connection) -> Result<(), String> {
    let current: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|error| format!("读取数据库版本失败：{error}"))?;
    let pending: Vec<&Migration> = MIGRATIONS
        .iter()
        .filter(|migration| migration.version > current)
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    // 只在需要迁移时做完整性检查，避免每次打开都全表扫描。
    let quick_check: String = connection
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|error| format!("数据库完整性检查失败：{error}"))?;
    if quick_check != "ok" {
        return Err(format!(
            "数据库完整性检查未通过（{quick_check}）。请先备份应用数据，或删除数据后重新归档。"
        ));
    }
    for migration in pending {
        let transaction = connection
            .transaction()
            .map_err(|error| format!("开始数据库迁移失败：{error}"))?;
        (migration.apply)(&transaction)
            .map_err(|error| format!("数据库迁移 {} 失败：{error}", migration.name))?;
        transaction
            .pragma_update(None, "user_version", migration.version)
            .map_err(|error| format!("记录数据库版本失败：{error}"))?;
        transaction
            .commit()
            .map_err(|error| format!("提交数据库迁移失败：{error}"))?;
    }
    Ok(())
}

/// 迁移 v1：存量 schema + 历史数据迁移（与旧版 open_database 行为等价）。
fn migration_v1(connection: &rusqlite::Transaction) -> Result<(), String> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS archive_feeds (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               owner_uin TEXT NOT NULL,
               feed_key TEXT NOT NULL,
               cell_id TEXT,
               event_type INTEGER NOT NULL DEFAULT 0,
               event_time INTEGER NOT NULL DEFAULT 0,
               title TEXT,
               content TEXT,
               event_summary TEXT,
               actor_uin TEXT,
               actor_name TEXT,
               original_author_uin TEXT,
               original_author_name TEXT,
               picture_count INTEGER NOT NULL DEFAULT 0,
               pictures_json TEXT,
               video_json TEXT,
               comments_json TEXT,
               raw_json TEXT NOT NULL,
               archived_at INTEGER NOT NULL,
               UNIQUE(owner_uin, feed_key)
             );
             CREATE INDEX IF NOT EXISTS idx_archive_feeds_owner_time
               ON archive_feeds(owner_uin, event_time DESC);
             CREATE INDEX IF NOT EXISTS idx_archive_feeds_dynamic_type
               ON archive_feeds(owner_uin, cell_id, event_type);
             CREATE TABLE IF NOT EXISTS archive_dynamics (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               owner_uin TEXT NOT NULL,
               cell_id TEXT NOT NULL,
               published_at INTEGER NOT NULL DEFAULT 0,
               content TEXT,
               author_uin TEXT,
               author_name TEXT,
               category TEXT NOT NULL DEFAULT '',
               pictures_json TEXT,
               video_json TEXT,
               raw_original_json TEXT NOT NULL,
               archived_at INTEGER NOT NULL,
               UNIQUE(owner_uin, cell_id)
             );
             CREATE INDEX IF NOT EXISTS idx_archive_dynamics_owner_time
               ON archive_dynamics(owner_uin, published_at DESC);
             CREATE TABLE IF NOT EXISTS archive_checkpoints (
               owner_uin TEXT PRIMARY KEY,
               attach_info TEXT NOT NULL,
               pages INTEGER NOT NULL DEFAULT 0,
               fetched INTEGER NOT NULL DEFAULT 0,
               saved INTEGER NOT NULL DEFAULT 0,
               updated_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS archive_rate_limits (
               owner_uin TEXT PRIMARY KEY,
               window_started_at INTEGER NOT NULL,
               requested_pages INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS archive_skips (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               owner_uin TEXT NOT NULL,
               cursor TEXT NOT NULL,
               resume_cursor TEXT NOT NULL,
               page_number INTEGER NOT NULL,
               cursor_offset INTEGER NOT NULL,
               offset_advance INTEGER NOT NULL,
               base_time INTEGER NOT NULL,
               error TEXT NOT NULL,
               skipped_at INTEGER NOT NULL,
               retry_count INTEGER NOT NULL DEFAULT 0,
               last_retry_at INTEGER,
               resolved_at INTEGER,
               recovered_records INTEGER NOT NULL DEFAULT 0,
               UNIQUE(owner_uin, cursor_offset, base_time)
             );",
        )
        .map_err(|error| format!("初始化归档数据库失败：{error}"))?;
    // 历史版本补列（幂等）
    if connection
        .prepare("SELECT pages,fetched,saved FROM archive_checkpoints LIMIT 0")
        .is_err()
    {
        connection
            .execute_batch(
                "ALTER TABLE archive_checkpoints ADD COLUMN pages INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE archive_checkpoints ADD COLUMN fetched INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE archive_checkpoints ADD COLUMN saved INTEGER NOT NULL DEFAULT 0;",
            )
            .map_err(|error| format!("升级归档续传统计失败：{error}"))?;
    }
    if connection
        .prepare("SELECT category FROM archive_dynamics LIMIT 0")
        .is_err()
    {
        connection
            .execute(
                "ALTER TABLE archive_dynamics ADD COLUMN category TEXT NOT NULL DEFAULT ''",
                [],
            )
            .map_err(|error| format!("升级归档分类失败：{error}"))?;
    }
    migrate_legacy_dynamics(connection)?;
    migrate_dynamic_categories(connection)?;
    Ok(())
}

/// 迁移 v2：统一模型扩展（Raw 层、数据源状态、用户、媒体、互动分表等）。
fn migration_v2(connection: &rusqlite::Transaction) -> Result<(), String> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS source_states (
               owner_uin TEXT NOT NULL,
               source TEXT NOT NULL,
               cursor TEXT NOT NULL DEFAULT '',
               status TEXT NOT NULL DEFAULT 'idle',
               last_sync_at INTEGER,
               next_sync_at INTEGER,
               last_error TEXT,
               total_fetched INTEGER NOT NULL DEFAULT 0,
               total_saved INTEGER NOT NULL DEFAULT 0,
               updated_at INTEGER NOT NULL DEFAULT 0,
               PRIMARY KEY (owner_uin, source)
             );
             CREATE TABLE IF NOT EXISTS raw_responses (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               owner_uin TEXT NOT NULL,
               source TEXT NOT NULL,
               method TEXT NOT NULL DEFAULT 'GET',
               url TEXT NOT NULL,
               query_json TEXT,
               status_code INTEGER NOT NULL,
               content_type TEXT,
               body TEXT,
               body_blob BLOB,
               body_sha256 TEXT NOT NULL,
               fetched_at INTEGER NOT NULL,
               UNIQUE (owner_uin, source, body_sha256)
             );
             CREATE TABLE IF NOT EXISTS users (
               uin TEXT PRIMARY KEY,
               nickname TEXT,
               avatar_url TEXT,
               first_seen_at INTEGER,
               last_seen_at INTEGER
             );
             CREATE TABLE IF NOT EXISTS dynamic_sources (
               dynamic_id INTEGER NOT NULL,
               owner_uin TEXT NOT NULL,
               source TEXT NOT NULL,
               raw_response_id INTEGER,
               matched_by TEXT NOT NULL DEFAULT 'platform_id',
               fetched_at INTEGER NOT NULL,
               PRIMARY KEY (dynamic_id, source)
             );
             CREATE TABLE IF NOT EXISTS merge_logs (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               owner_uin TEXT NOT NULL,
               dynamic_id INTEGER NOT NULL,
               source TEXT NOT NULL,
               matched_by TEXT NOT NULL,
               note TEXT,
               created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS media_items (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               owner_uin TEXT NOT NULL,
               dynamic_id INTEGER,
               media_kind TEXT NOT NULL,
               remote_url TEXT NOT NULL,
               local_path TEXT,
               sha256 TEXT,
               size_bytes INTEGER,
               mime_type TEXT,
               download_status TEXT NOT NULL DEFAULT 'pending',
               download_attempts INTEGER NOT NULL DEFAULT 0,
               last_error TEXT,
               last_downloaded_at INTEGER,
               created_at INTEGER NOT NULL,
               UNIQUE (owner_uin, media_kind, remote_url)
             );
             CREATE INDEX IF NOT EXISTS idx_media_items_owner_status
               ON media_items(owner_uin, download_status);
             CREATE TABLE IF NOT EXISTS comments (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               owner_uin TEXT NOT NULL,
               dynamic_id INTEGER NOT NULL,
               comment_id TEXT,
               uin TEXT,
               nickname TEXT,
               content TEXT NOT NULL,
               created_at INTEGER NOT NULL DEFAULT 0,
               raw_json TEXT,
               UNIQUE (owner_uin, dynamic_id, comment_id)
             );
             CREATE TABLE IF NOT EXISTS likes (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               owner_uin TEXT NOT NULL,
               dynamic_id INTEGER NOT NULL,
               uin TEXT,
               nickname TEXT,
               created_at INTEGER NOT NULL DEFAULT 0,
               UNIQUE (owner_uin, dynamic_id, uin)
             );
             CREATE TABLE IF NOT EXISTS replies (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               owner_uin TEXT NOT NULL,
               dynamic_id INTEGER NOT NULL,
               comment_id TEXT,
               uin TEXT,
               nickname TEXT,
               reply_to_uin TEXT,
               reply_to_nickname TEXT,
               content TEXT NOT NULL,
               created_at INTEGER NOT NULL DEFAULT 0,
               raw_json TEXT
             );
             CREATE TABLE IF NOT EXISTS guestbook_entries (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               owner_uin TEXT NOT NULL,
               entry_id TEXT,
               author_uin TEXT,
               author_name TEXT,
               content TEXT,
               created_at INTEGER NOT NULL DEFAULT 0,
               raw_json TEXT,
               UNIQUE (owner_uin, entry_id)
             );
             CREATE TABLE IF NOT EXISTS albums (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               owner_uin TEXT NOT NULL,
               album_id TEXT NOT NULL,
               name TEXT,
               description TEXT,
               photo_count INTEGER,
               created_at INTEGER,
               raw_json TEXT,
               UNIQUE (owner_uin, album_id)
             );
             CREATE TABLE IF NOT EXISTS album_photos (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               owner_uin TEXT NOT NULL,
               album_id TEXT,
               photo_id TEXT,
               url TEXT,
               raw_url TEXT,
               width INTEGER,
               height INTEGER,
               taken_at INTEGER,
               raw_json TEXT,
               UNIQUE (owner_uin, album_id, photo_id)
             );
             CREATE TABLE IF NOT EXISTS archive_settings (
               key TEXT PRIMARY KEY,
               value TEXT,
               updated_at INTEGER NOT NULL
             );",
        )
        .map_err(|error| format!("创建统一模型扩展表失败：{error}"))?;
    // archive_dynamics 扩展列（幂等补列）
    if connection
        .prepare("SELECT source FROM archive_dynamics LIMIT 0")
        .is_err()
    {
        connection
            .execute(
                "ALTER TABLE archive_dynamics ADD COLUMN source TEXT NOT NULL DEFAULT 'feeds'",
                [],
            )
            .map_err(|error| format!("升级动态来源列失败：{error}"))?;
    }
    if connection
        .prepare("SELECT content_fingerprint FROM archive_dynamics LIMIT 0")
        .is_err()
    {
        connection
            .execute(
                "ALTER TABLE archive_dynamics ADD COLUMN content_fingerprint TEXT",
                [],
            )
            .map_err(|error| format!("升级动态指纹列失败：{error}"))?;
    }
    if connection
        .prepare("SELECT remote_status FROM archive_dynamics LIMIT 0")
        .is_err()
    {
        connection
            .execute(
                "ALTER TABLE archive_dynamics ADD COLUMN remote_status TEXT NOT NULL DEFAULT 'active'",
                [],
            )
            .map_err(|error| format!("升级动态远端状态列失败：{error}"))?;
    }
    if connection
        .prepare("SELECT first_seen_at FROM archive_dynamics LIMIT 0")
        .is_err()
    {
        connection
            .execute(
                "ALTER TABLE archive_dynamics ADD COLUMN first_seen_at INTEGER",
                [],
            )
            .map_err(|error| format!("升级动态首次发现列失败：{error}"))?;
    }
    if connection
        .prepare("SELECT last_seen_at FROM archive_dynamics LIMIT 0")
        .is_err()
    {
        connection
            .execute(
                "ALTER TABLE archive_dynamics ADD COLUMN last_seen_at INTEGER",
                [],
            )
            .map_err(|error| format!("升级动态最近发现列失败：{error}"))?;
    }
    Ok(())
}

/// 迁移 v3：albums 表补远端状态列（幂等），支撑远端消失标记。
fn migration_v3(connection: &rusqlite::Transaction) -> Result<(), String> {
    if connection
        .prepare("SELECT remote_status FROM albums LIMIT 0")
        .is_err()
    {
        connection
            .execute(
                "ALTER TABLE albums ADD COLUMN remote_status TEXT NOT NULL DEFAULT 'active'",
                [],
            )
            .map_err(|error| format!("升级相册远端状态列失败：{error}"))?;
    }
    if connection
        .prepare("SELECT last_seen_at FROM albums LIMIT 0")
        .is_err()
    {
        connection
            .execute("ALTER TABLE albums ADD COLUMN last_seen_at INTEGER", [])
            .map_err(|error| format!("升级相册最近发现列失败：{error}"))?;
    }
    Ok(())
}

/// 历史迁移：archive_dynamics 为空时从 archive_feeds.raw_json 重建动态。
fn migrate_legacy_dynamics(connection: &rusqlite::Transaction) -> Result<(), String> {
    let dynamic_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM archive_dynamics", [], |row| {
            row.get(0)
        })
        .map_err(|error| format!("检查原动态迁移状态失败：{error}"))?;
    if dynamic_count > 0 {
        return Ok(());
    }
    let legacy = {
        let mut statement = connection
            .prepare("SELECT owner_uin,raw_json FROM archive_feeds")
            .map_err(|error| format!("读取旧归档失败：{error}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| format!("查询旧归档失败：{error}"))?;
        rows.filter_map(Result::ok).collect::<Vec<_>>()
    };
    if legacy.is_empty() {
        return Ok(());
    }
    for (owner_uin, raw_json) in legacy {
        if let Ok(feed) = serde_json::from_str::<serde_json::Value>(&raw_json) {
            model::save_dynamic(connection, &owner_uin, &feed, None, "feeds")?;
        }
    }
    Ok(())
}

/// 历史迁移：补全 archive_dynamics.category（旧数据 category 为空）。
fn migrate_dynamic_categories(connection: &rusqlite::Transaction) -> Result<(), String> {
    let pending: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM archive_dynamics WHERE category=''",
            [],
            |row| row.get(0),
        )
        .map_err(|error| format!("检查归档分类迁移状态失败：{error}"))?;
    if pending == 0 {
        return Ok(());
    }
    let feeds = {
        let mut statement = connection
            .prepare("SELECT owner_uin,raw_json FROM archive_feeds")
            .map_err(|error| format!("读取待分类归档失败：{error}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| format!("查询待分类归档失败：{error}"))?;
        rows.filter_map(Result::ok).collect::<Vec<_>>()
    };
    for (owner_uin, raw_json) in feeds {
        if let Ok(feed) = serde_json::from_str::<serde_json::Value>(&raw_json) {
            model::save_dynamic(connection, &owner_uin, &feed, None, "feeds")?;
        }
    }
    connection.execute(
        "UPDATE archive_dynamics SET category=CASE WHEN author_uin=owner_uin THEN 'self' ELSE 'other' END WHERE category=''",
        [],
    ).map_err(|error| format!("补全归档分类失败：{error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{open_database_at, MIGRATIONS, SCHEMA_VERSION};
    use rusqlite::Connection;

    fn temp_db_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("qza-migration-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!(
            "{}-{}-{}.sqlite3",
            name,
            std::process::id(),
            crate::util::now_millis()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn table_names(conn: &Connection) -> Vec<String> {
        let mut statement = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap();
        rows.filter_map(Result::ok).collect()
    }

    fn column_names(conn: &Connection, table: &str) -> Vec<String> {
        let mut statement = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        let rows = statement
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap();
        rows.filter_map(Result::ok).collect()
    }

    #[test]
    fn fresh_database_reaches_latest_version() {
        let path = temp_db_path("fresh");
        let connection = open_database_at(&path).unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let tables = table_names(&connection);
        for required in [
            "archive_feeds",
            "archive_dynamics",
            "raw_responses",
            "media_items",
            "source_states",
            "users",
            "comments",
            "likes",
            "replies",
            "guestbook_entries",
            "albums",
            "album_photos",
            "archive_settings",
            "dynamic_sources",
            "merge_logs",
        ] {
            assert!(tables.iter().any(|t| t == required), "缺少表 {required}");
        }
        let dynamic_columns = column_names(&connection, "archive_dynamics");
        for required in [
            "source",
            "content_fingerprint",
            "remote_status",
            "first_seen_at",
            "last_seen_at",
        ] {
            assert!(
                dynamic_columns.iter().any(|c| c == required),
                "archive_dynamics 缺少列 {required}"
            );
        }
        let album_columns = column_names(&connection, "albums");
        for required in ["remote_status", "last_seen_at"] {
            assert!(
                album_columns.iter().any(|c| c == required),
                "albums 缺少列 {required}"
            );
        }
    }

    #[test]
    fn migration_is_idempotent() {
        let path = temp_db_path("idempotent");
        {
            let connection = open_database_at(&path).unwrap();
            connection
                .execute(
                    "INSERT INTO archive_settings(key,value,updated_at) VALUES ('a','b',1)",
                    [],
                )
                .unwrap();
        }
        let connection = open_database_at(&path).unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn legacy_database_upgrades_without_losing_rows() {
        let path = temp_db_path("legacy-upgrade");
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE archive_feeds (
                       id INTEGER PRIMARY KEY AUTOINCREMENT,
                       owner_uin TEXT NOT NULL, feed_key TEXT NOT NULL,
                       cell_id TEXT, event_type INTEGER NOT NULL DEFAULT 0,
                       event_time INTEGER NOT NULL DEFAULT 0, title TEXT, content TEXT,
                       event_summary TEXT, actor_uin TEXT, actor_name TEXT,
                       original_author_uin TEXT, original_author_name TEXT,
                       picture_count INTEGER NOT NULL DEFAULT 0,
                       pictures_json TEXT, video_json TEXT, comments_json TEXT,
                       raw_json TEXT NOT NULL, archived_at INTEGER NOT NULL,
                       UNIQUE(owner_uin, feed_key)
                     );
                     CREATE TABLE archive_dynamics (
                       id INTEGER PRIMARY KEY AUTOINCREMENT,
                       owner_uin TEXT NOT NULL, cell_id TEXT NOT NULL,
                       published_at INTEGER NOT NULL DEFAULT 0, content TEXT,
                       author_uin TEXT, author_name TEXT, category TEXT NOT NULL DEFAULT '',
                       pictures_json TEXT, video_json TEXT,
                       raw_original_json TEXT NOT NULL, archived_at INTEGER NOT NULL,
                       UNIQUE(owner_uin, cell_id)
                     );
                     CREATE TABLE archive_checkpoints (
                       owner_uin TEXT PRIMARY KEY, attach_info TEXT NOT NULL,
                       updated_at INTEGER NOT NULL
                     );
                     CREATE TABLE archive_rate_limits (
                       owner_uin TEXT PRIMARY KEY,
                       window_started_at INTEGER NOT NULL,
                       requested_pages INTEGER NOT NULL DEFAULT 0
                     );
                     CREATE TABLE archive_skips (
                       id INTEGER PRIMARY KEY AUTOINCREMENT,
                       owner_uin TEXT NOT NULL, cursor TEXT NOT NULL,
                       resume_cursor TEXT NOT NULL, page_number INTEGER NOT NULL,
                       cursor_offset INTEGER NOT NULL, offset_advance INTEGER NOT NULL,
                       base_time INTEGER NOT NULL, error TEXT NOT NULL,
                       skipped_at INTEGER NOT NULL, retry_count INTEGER NOT NULL DEFAULT 0,
                       last_retry_at INTEGER, resolved_at INTEGER,
                       recovered_records INTEGER NOT NULL DEFAULT 0
                     );",
                )
                .unwrap();
            connection.execute(
                "INSERT INTO archive_dynamics
                 (owner_uin,cell_id,published_at,content,author_uin,author_name,category,raw_original_json,archived_at)
                 VALUES ('10001','cell-1',100,'你好','10001','我','self','{}',200)",
                [],
            ).unwrap();
            connection.execute(
                "INSERT INTO archive_feeds
                 (owner_uin,feed_key,cell_id,event_type,event_time,actor_uin,actor_name,raw_json,archived_at)
                 VALUES ('10001','1_1','cell-1',1,100,'10002','友','{}',200)",
                [],
            ).unwrap();
        }
        let connection = open_database_at(&path).unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM archive_dynamics", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
        let content: String = connection
            .query_row(
                "SELECT content FROM archive_dynamics WHERE cell_id='cell-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(content, "你好");
        let source: String = connection
            .query_row(
                "SELECT source FROM archive_dynamics WHERE cell_id='cell-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(source, "feeds");
        let status: String = connection
            .query_row(
                "SELECT remote_status FROM archive_dynamics WHERE cell_id='cell-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "active");
    }

    #[test]
    fn migration_list_is_ordered_and_contiguous() {
        for (index, migration) in MIGRATIONS.iter().enumerate() {
            assert_eq!(
                migration.version as usize,
                index + 1,
                "版本必须从 1 开始连续"
            );
        }
    }

    #[test]
    fn keeps_legacy_data_after_upgrade_with_raw_and_sources() {
        let path = temp_db_path("v2-writable");
        let connection = open_database_at(&path).unwrap();
        connection
            .execute(
                "INSERT INTO source_states(owner_uin,source,updated_at) VALUES ('10001','feeds',1)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO raw_responses
                 (owner_uin,source,method,url,status_code,body_sha256,fetched_at)
                 VALUES ('10001','feeds','GET','https://example.test/',200,'abc',1)",
                [],
            )
            .unwrap();
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM raw_responses", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }
}
