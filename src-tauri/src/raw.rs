//! Raw 数据留存层：保存接口返回的原始 JSON / JSONP / HTML 响应，
//! 与标准化数据库层并存。以后解析规则变化时，可从 Raw 层重建归档。
//!
//! 安全约束：
//! - 只记录响应本身与请求元数据；绝不记录 Cookie 头或其它凭证头；
//! - 请求查询参数中的敏感项（skey/token/sig/pwd2sig 等）会被剔除；
//! - body 按 SHA-256 去重，同一 (owner_uin, source) 下重复响应不重复入库。

use rusqlite::{params, Connection};
use serde_json::Value;

use crate::util::{body_sql_value, now, sha256_hex};

/// 一次接口原始响应。
pub struct RawRecord {
    pub owner_uin: String,
    /// 数据源标识，如 "feeds"、"album_list"、"shuoshuo"。
    pub source: String,
    pub method: String,
    pub url: String,
    /// 请求查询参数（已脱敏），可为空。
    pub query: Option<Value>,
    pub status_code: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

/// 保存一条原始响应（按 body SHA-256 去重）。返回 raw_responses.id。
pub fn save_raw(conn: &Connection, record: &RawRecord) -> Result<i64, String> {
    let hash = sha256_hex(&record.body);
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM raw_responses WHERE owner_uin=?1 AND source=?2 AND body_sha256=?3",
            params![record.owner_uin, record.source, hash],
            |row| row.get(0),
        )
        .ok();
    if let Some(id) = existing {
        return Ok(id);
    }
    let (body_text, body_blob) = body_sql_value(&record.body);
    conn.execute(
        "INSERT INTO raw_responses
         (owner_uin,source,method,url,query_json,status_code,content_type,body,body_blob,body_sha256,fetched_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        params![
            record.owner_uin,
            record.source,
            record.method,
            record.url,
            record.query.as_ref().map(Value::to_string),
            record.status_code,
            record.content_type,
            body_text,
            body_blob,
            hash,
            now(),
        ],
    )
    .map_err(|error| format!("保存原始响应失败：{error}"))?;
    Ok(conn.last_insert_rowid())
}

/// 剔除查询参数中的凭证类字段（Cookie/skey/token/sig 等）。
/// 仅用于日志与诊断；Raw 库中仍保留完整参数以便复现请求。
pub fn redact_query_for_diagnostic(query: &[(String, String)]) -> Value {
    const SENSITIVE_KEYS: &[&str] = &[
        "skey", "p_skey", "pt4_token", "pwd2sig", "pwd2Sig", "sig", "token", "ptsigx",
        "login_sig", "qrsig",
    ];
    let map: serde_json::Map<String, Value> = query
        .iter()
        .map(|(key, value)| {
            let redacted = SENSITIVE_KEYS
                .iter()
                .any(|sensitive| key.eq_ignore_ascii_case(sensitive) || key.contains(sensitive));
            (
                key.clone(),
                Value::String(if redacted {
                    "[redacted]".to_owned()
                } else {
                    value.clone()
                }),
            )
        })
        .collect();
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::redact_query_for_diagnostic;

    #[test]
    fn redacts_credential_like_query_params() {
        let query = vec![
            ("g_tk".to_owned(), "123".to_owned()),
            ("uin".to_owned(), "10001".to_owned()),
            ("p_skey".to_owned(), "secret".to_owned()),
            ("res_attach".to_owned(), "cursor".to_owned()),
        ];
        let redacted = redact_query_for_diagnostic(&query);
        assert_eq!(redacted["g_tk"], "123");
        assert_eq!(redacted["uin"], "10001");
        assert_eq!(redacted["p_skey"], "[redacted]");
        assert_eq!(redacted["res_attach"], "cursor");
    }
}
