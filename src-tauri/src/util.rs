//! 共享小工具：时间、哈希、JSON 取值。不含任何登录凭证逻辑。
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sha2::{Digest, Sha256};

/// 当前 Unix 秒。
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// 当前 Unix 毫秒。
#[allow(dead_code)]
pub fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// SHA-256 hex（小写）。用于 Raw 响应去重、媒体文件去重与内容指纹。
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// 从 JSON 取值：字符串（非空）或数字，统一转为 String。
pub fn text_at(value: &Value, pointer: &str) -> Option<String> {
    value.pointer(pointer).and_then(|value| match value {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    })
}

/// 从 JSON 中读取 i64（含字符串形式）。
#[allow(dead_code)]
pub fn int_at(value: &Value, pointer: &str) -> Option<i64> {
    value.pointer(pointer).and_then(|value| match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.parse::<i64>().ok(),
        _ => None,
    })
}

/// 把响应体转为 SQLite 可存值：UTF-8 用 TEXT，否则用 BLOB。
pub fn body_sql_value(body: &[u8]) -> (Option<String>, Option<Vec<u8>>) {
    match std::str::from_utf8(body) {
        Ok(text) => (Some(text.to_owned()), None),
        Err(_) => (None, Some(body.to_vec())),
    }
}

#[cfg(test)]
mod tests {
    use super::{body_sql_value, int_at, sha256_hex, text_at};
    use serde_json::json;

    #[test]
    fn hashes_are_stable_and_lowercase_hex() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn extracts_text_and_numbers_from_json_pointers() {
        let value = json!({"a": {"b": "hi"}, "n": 42, "s": "7"});
        assert_eq!(text_at(&value, "/a/b").as_deref(), Some("hi"));
        assert_eq!(text_at(&value, "/n").as_deref(), Some("42"));
        assert_eq!(text_at(&value, "/missing"), None);
        assert_eq!(text_at(&value, "/a"), None); // 对象不是文本
        assert_eq!(int_at(&value, "/n"), Some(42));
        assert_eq!(int_at(&value, "/s"), Some(7));
    }

    #[test]
    fn splits_utf8_and_binary_bodies() {
        let (text, blob) = body_sql_value("你好".as_bytes());
        assert_eq!(text.as_deref(), Some("你好"));
        assert!(blob.is_none());
        let (text, blob) = body_sql_value(&[0xff, 0x00, 0xfe]);
        assert!(text.is_none());
        assert_eq!(blob.as_deref(), Some(&[0xff, 0x00, 0xfe][..]));
    }
}
