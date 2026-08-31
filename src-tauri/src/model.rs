//! 统一数据模型层：动态（archive_dynamics 增强）、用户、评论、点赞、回复、
//! 留言、动态来源与内容指纹。相同内容来自多个接口时按平台 ID 合并；
//! 缺少可靠 ID 时用指纹候选匹配（Phase 2 跨源合并接入，工具函数在此）。
//!
//! 设计约束：本层只依赖 rusqlite / serde_json / util，不依赖 archive / qzone，
//! 以便 db 迁移与 Raw 重建流程复用同一套写入逻辑。

use std::collections::HashSet;

use rusqlite::{params, Transaction};
use serde::Serialize;
use serde_json::Value;

use crate::util::{now, sha256_hex, text_at};

/// 存档动态（统一模型中的动态实体，向后兼容 archive_dynamics 结构）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveComment {
    #[serde(skip)]
    pub comment_id: Option<String>,
    pub uin: Option<String>,
    pub nickname: Option<String>,
    pub content: String,
    pub created_at: i64,
    pub replies: Vec<ArchiveReply>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveReply {
    pub uin: Option<String>,
    pub nickname: Option<String>,
    pub reply_to_uin: Option<String>,
    pub reply_to_nickname: Option<String>,
    pub content: String,
    pub created_at: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LikeUser {
    pub uin: Option<String>,
    pub nickname: Option<String>,
}

/// 内容指纹：作者 + 发布时间 + 正文 + 媒体 URL 集合。用于跨数据源候选匹配。
pub fn content_fingerprint(
    author_uin: Option<&str>,
    published_at: i64,
    content: Option<&str>,
    pictures_json: Option<&str>,
    video_json: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(author_uin.unwrap_or("").to_owned());
    parts.push(published_at.to_string());
    parts.push(content.unwrap_or("").trim().to_owned());
    for url in picture_urls(pictures_json.map(str::to_owned)) {
        parts.push(url);
    }
    for url in video_urls(video_json.map(str::to_owned)) {
        parts.push(url);
    }
    sha256_hex(parts.join("\u{1f}").as_bytes())
}

/// 媒体 URL 提取：图片候选组、图片首选 URL、视频 URL、视频封面 URL。
pub fn picture_url_candidates(json: Option<String>) -> Vec<Vec<String>> {
    let Some(value) = json.and_then(|text| serde_json::from_str::<Value>(&text).ok()) else {
        return vec![];
    };
    value
        .pointer("/picdata/pic")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|pic| {
            let photo_urls = pic.get("photourl")?;
            let values = match photo_urls {
                Value::Array(items) => items.iter().collect::<Vec<_>>(),
                Value::Object(items) => items.values().collect::<Vec<_>>(),
                _ => vec![],
            };
            let candidates = values
                .into_iter()
                .filter_map(|item| {
                    let url = item.get("url")?.as_str()?.trim();
                    if url.is_empty() {
                        return None;
                    }
                    Some(url.to_owned())
                })
                .collect::<Vec<_>>();
            let mut candidates = candidates;
            if let Some(url) = pic
                .pointer("/busi_param/-1")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|url| !url.is_empty())
            {
                candidates.push(url.to_owned());
            }
            let mut seen = HashSet::new();
            let urls = candidates
                .into_iter()
                .map(|url| {
                    if url.starts_with("//") {
                        format!("https:{url}")
                    } else {
                        url
                    }
                })
                .filter(|url| seen.insert(url.clone()))
                .collect::<Vec<_>>();
            (!urls.is_empty()).then_some(urls)
        })
        .collect()
}

/// 每张图片的首选 URL（与旧版行为一致）。
pub fn picture_urls(json: Option<String>) -> Vec<String> {
    picture_url_candidates(json)
        .into_iter()
        .filter_map(|urls| urls.into_iter().next())
        .collect()
}

pub fn video_urls(json: Option<String>) -> Vec<String> {
    let Some(value) = json.and_then(|text| serde_json::from_str::<Value>(&text).ok()) else {
        return vec![];
    };
    let mut urls = Vec::new();
    if let Some(url) = value.get("videourl").and_then(Value::as_str) {
        urls.push(url.to_owned());
    }
    if let Some(items) = value.get("videourls").and_then(Value::as_object) {
        for url in items
            .values()
            .filter_map(|item| item.get("url").and_then(Value::as_str))
        {
            if !urls.iter().any(|saved| saved == url) {
                urls.push(url.to_owned());
            }
        }
    }
    urls
}

pub fn video_cover_url(json: Option<String>) -> Option<String> {
    let value = json.and_then(|text| serde_json::from_str::<Value>(&text).ok())?;
    value
        .pointer("/coverurl/0/url")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("coverurl")?
                .as_object()?
                .values()
                .find_map(|item| item.get("url")?.as_str())
        })
        .map(str::to_owned)
}

fn is_guestbook_feed(original: &Value) -> bool {
    let original_appid = original
        .pointer("/cell_comm/appid")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let original_key = text_at(original, "/cell_comm/feedskey").unwrap_or_default();
    original_appid == 334 || original_key.starts_with("334_")
}

fn dynamic_exists(tx: &Transaction<'_>, owner_uin: &str, cell_id: &str) -> Result<bool, String> {
    let count: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM archive_dynamics WHERE owner_uin=?1 AND cell_id=?2",
            params![owner_uin, cell_id],
            |row| row.get(0),
        )
        .map_err(|error| format!("检查动态是否存在失败：{error}"))?;
    Ok(count > 0)
}

/// 保存一条动态（含统一模型扩展行）。feed 需包含 original 子对象。
/// 返回 archive_dynamics 行 id；无 original 时返回 None。
pub fn save_dynamic(
    tx: &Transaction<'_>,
    owner_uin: &str,
    feed: &Value,
    raw_response_id: Option<i64>,
    source: &str,
) -> Result<Option<i64>, String> {
    let Some(original) = feed.get("original") else {
        return Ok(None);
    };
    let Some(cell_id) = text_at(original, "/cell_id/cellid") else {
        return Ok(None);
    };
    let is_guestbook = is_guestbook_feed(original);
    let published_at = original
        .pointer("/cell_comm/time")
        .and_then(Value::as_i64)
        .or_else(|| feed.pointer("/comm/time").and_then(Value::as_i64))
        .unwrap_or(0);
    let content = if is_guestbook {
        text_at(feed, "/summary/summary")
    } else {
        text_at(original, "/cell_summary/summary")
    };
    let author_uin = if is_guestbook {
        text_at(feed, "/userinfo/user/uin")
    } else {
        text_at(original, "/cell_userinfo/user/uin")
    };
    let author_name = if is_guestbook {
        text_at(feed, "/userinfo/user/nickname")
    } else {
        text_at(original, "/cell_userinfo/user/nickname")
    };
    let category = if is_guestbook {
        "guestbook"
    } else if author_uin.as_deref() == Some(owner_uin) {
        "self"
    } else {
        "other"
    };
    let pictures_json = original
        .get("cell_pic")
        .filter(|value| !value.is_null())
        .map(Value::to_string);
    let video_json = original
        .get("cell_video")
        .filter(|value| !value.is_null())
        .map(Value::to_string);
    let fingerprint = content_fingerprint(
        author_uin.as_deref(),
        published_at,
        content.as_deref(),
        pictures_json.as_deref(),
        video_json.as_deref(),
    );
    let existed = dynamic_exists(tx, owner_uin, &cell_id)?;
    let now_value = now();
    let id: i64 = tx.query_row(
        "INSERT INTO archive_dynamics
         (owner_uin,cell_id,published_at,content,author_uin,author_name,category,pictures_json,video_json,
          raw_original_json,archived_at,source,content_fingerprint,remote_status,first_seen_at,last_seen_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,'active',?14,?14)
         ON CONFLICT(owner_uin,cell_id) DO UPDATE SET
          published_at=excluded.published_at,content=excluded.content,author_uin=excluded.author_uin,
          author_name=excluded.author_name,category=excluded.category,
          pictures_json=COALESCE(excluded.pictures_json,archive_dynamics.pictures_json),
          video_json=COALESCE(excluded.video_json,archive_dynamics.video_json),
          raw_original_json=excluded.raw_original_json,archived_at=excluded.archived_at,
          source=CASE WHEN archive_dynamics.source IS NULL OR archive_dynamics.source='' THEN excluded.source ELSE archive_dynamics.source END,
          content_fingerprint=excluded.content_fingerprint,last_seen_at=excluded.last_seen_at
         RETURNING id",
        params![
            owner_uin,
            cell_id,
            published_at,
            content,
            author_uin,
            author_name,
            category,
            pictures_json,
            video_json,
            original.to_string(),
            now_value,
            source,
            fingerprint,
            now_value,
        ],
        |row| row.get(0),
    )
    .map_err(|error| format!("保存原动态失败：{error}"))?;

    // 动态 × 数据来源（保留合并前来源，禁止因去重丢失来源）
    tx.execute(
        "INSERT INTO dynamic_sources(dynamic_id,owner_uin,source,raw_response_id,matched_by,fetched_at)
         VALUES (?1,?2,?3,?4,?5,?6)
         ON CONFLICT(dynamic_id,source) DO UPDATE SET
          raw_response_id=excluded.raw_response_id,matched_by=excluded.matched_by,fetched_at=excluded.fetched_at",
        params![
            id,
            owner_uin,
            source,
            raw_response_id,
            if existed { "platform_id" } else { "new" },
            now_value,
        ],
    )
    .map_err(|error| format!("记录动态来源失败：{error}"))?;

    if let Some(author_uin) = author_uin.as_deref() {
        upsert_user(tx, author_uin, author_name.as_deref(), None)?;
    }
    write_feed_interactions(tx, owner_uin, id, feed, &cell_id, is_guestbook)?;
    Ok(Some(id))
}

fn upsert_user(
    tx: &Transaction<'_>,
    uin: &str,
    nickname: Option<&str>,
    avatar_url: Option<&str>,
) -> Result<(), String> {
    if uin.trim().is_empty() {
        return Ok(());
    }
    let now_value = now();
    tx.execute(
        "INSERT INTO users(uin,nickname,avatar_url,first_seen_at,last_seen_at)
         VALUES (?1,?2,?3,?4,?4)
         ON CONFLICT(uin) DO UPDATE SET
          nickname=COALESCE(NULLIF(excluded.nickname,''),users.nickname),
          avatar_url=COALESCE(excluded.avatar_url,users.avatar_url),
          last_seen_at=excluded.last_seen_at",
        params![uin, nickname, avatar_url, now_value],
    )
    .map_err(|error| format!("保存用户失败：{error}"))?;
    Ok(())
}

/// 从单条 feed（互动事件）提取并写入评论 / 点赞 / 回复行。
/// 同一动态的多个互动事件会收敛到同一组行（按 ID 或内容去重）。
fn write_feed_interactions(
    tx: &Transaction<'_>,
    owner_uin: &str,
    dynamic_id: i64,
    feed: &Value,
    cell_id: &str,
    is_guestbook: bool,
) -> Result<(), String> {
    let event_type = feed
        .pointer("/comm/subid")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let actor_uin = text_at(feed, "/userinfo/user/uin");
    let actor_name = text_at(feed, "/userinfo/user/nickname");
    let event_time = feed
        .pointer("/comm/time")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let event_summary = text_at(feed, "/summary/summary");

    // 互动参与者（点赞/评论/回复用户）统一登记到 users 表
    if let Some(uin) = actor_uin.as_deref() {
        upsert_user(tx, uin, actor_name.as_deref(), None)?;
    }
    if let Some(comments_json) = feed
        .pointer("/original/cell_comment")
        .filter(|value| !value.is_null())
        .map(Value::to_string)
    {
        let comment = comment_from_values(
            Some(comments_json),
            actor_uin.clone(),
            actor_name.clone(),
            event_summary.clone(),
            event_time,
        );
        if let Some(uin) = comment.uin.as_deref() {
            upsert_user(tx, uin, comment.nickname.as_deref(), None)?;
        }
        for reply in &comment.replies {
            if let Some(uin) = reply.uin.as_deref() {
                upsert_user(tx, uin, reply.nickname.as_deref(), None)?;
            }
        }
    }

    if event_type == 217 {
        if let Some(uin) = actor_uin.as_deref() {
            tx.execute(
                "INSERT INTO likes(owner_uin,dynamic_id,uin,nickname,created_at)
                 VALUES (?1,?2,?3,?4,?5)
                 ON CONFLICT(owner_uin,dynamic_id,uin) DO UPDATE SET
                  nickname=COALESCE(NULLIF(excluded.nickname,''),likes.nickname),
                  created_at=excluded.created_at",
                params![owner_uin, dynamic_id, uin, actor_name, event_time],
            )
            .map_err(|error| format!("保存点赞记录失败：{error}"))?;
        }
    }

    let comments_json = feed
        .pointer("/original/cell_comment")
        .filter(|value| !value.is_null())
        .map(Value::to_string);
    if let Some(comments_json) = comments_json {
        let comment = comment_from_values(
            Some(comments_json),
            actor_uin.clone(),
            actor_name.clone(),
            event_summary.clone(),
            event_time,
        );
        upsert_comment(tx, owner_uin, dynamic_id, &comment)?;
        for reply in &comment.replies {
            tx.execute(
                "INSERT INTO replies
                 (owner_uin,dynamic_id,comment_id,uin,nickname,reply_to_uin,reply_to_nickname,content,created_at,raw_json)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,NULL)
                 ON CONFLICT DO NOTHING",
                params![
                    owner_uin,
                    dynamic_id,
                    comment.comment_id,
                    reply.uin,
                    reply.nickname,
                    reply.reply_to_uin,
                    reply.reply_to_nickname,
                    reply.content,
                    reply.created_at,
                ],
            )
            .map_err(|error| format!("保存评论回复失败：{error}"))?;
        }
    }

    // 留言板动态：写入统一留言表（来源 = feeds 互动列表）。
    if is_guestbook {
        tx.execute(
            "INSERT INTO guestbook_entries(owner_uin,entry_id,author_uin,author_name,content,created_at,raw_json)
             VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(owner_uin,entry_id) DO UPDATE SET
              author_uin=excluded.author_uin,author_name=excluded.author_name,
              content=excluded.content,created_at=excluded.created_at,raw_json=excluded.raw_json",
            params![
                owner_uin,
                cell_id,
                actor_uin,
                actor_name,
                event_summary.unwrap_or_default(),
                event_time,
                feed.get("original").map(Value::to_string),
            ],
        )
        .map_err(|error| format!("保存留言记录失败：{error}"))?;
    }
    Ok(())
}

/// 写评论行：有 comment_id 时按 ID 去重，无 ID 时按（作者,内容,时间）候选去重。
fn upsert_comment(
    tx: &Transaction<'_>,
    owner_uin: &str,
    dynamic_id: i64,
    comment: &ArchiveComment,
) -> Result<(), String> {
    let comment_id = comment.comment_id.clone();
    match comment_id.as_deref() {
        Some(id) => {
            tx.execute(
                "INSERT INTO comments(owner_uin,dynamic_id,comment_id,uin,nickname,content,created_at,raw_json)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,NULL)
                 ON CONFLICT(owner_uin,dynamic_id,comment_id) DO UPDATE SET
                  uin=excluded.uin,nickname=COALESCE(NULLIF(excluded.nickname,''),comments.nickname),
                  content=excluded.content,created_at=excluded.created_at",
                params![
                    owner_uin,
                    dynamic_id,
                    id,
                    comment.uin,
                    comment.nickname,
                    comment.content,
                    comment.created_at,
                ],
            )
            .map_err(|error| format!("保存评论失败：{error}"))?;
        }
        None => {
            let existing: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM comments
                     WHERE owner_uin=?1 AND dynamic_id=?2 AND uin=?3 AND content=?4 AND created_at=?5",
                    params![
                        owner_uin,
                        dynamic_id,
                        comment.uin,
                        comment.content,
                        comment.created_at
                    ],
                    |row| row.get(0),
                )
                .map_err(|error| format!("检查评论去重失败：{error}"))?;
            if existing == 0 {
                tx.execute(
                    "INSERT INTO comments(owner_uin,dynamic_id,comment_id,uin,nickname,content,created_at,raw_json)
                     VALUES (?1,?2,NULL,?3,?4,?5,?6,NULL)",
                    params![
                        owner_uin,
                        dynamic_id,
                        comment.uin,
                        comment.nickname,
                        comment.content,
                        comment.created_at,
                    ],
                )
                .map_err(|error| format!("保存评论失败：{error}"))?;
            }
        }
    }
    Ok(())
}

/// 从 cell_comment JSON + feed 级回退字段构造评论对象（与旧版渲染逻辑一致）。
pub fn comment_from_values(
    json: Option<String>,
    fallback_uin: Option<String>,
    fallback_name: Option<String>,
    fallback_content: Option<String>,
    fallback_time: i64,
) -> ArchiveComment {
    let value = json.and_then(|text| serde_json::from_str::<Value>(&text).ok());
    let main = value.as_ref().and_then(|value| value.get("main_comment"));
    let comment_id = main.and_then(|value| text_at(value, "/commentid"));
    let main_uin = main.and_then(|value| text_at(value, "/user/uin"));
    let main_name = main.and_then(|value| text_at(value, "/user/nickname"));
    let main_content = main.and_then(|value| text_at(value, "/content"));
    let main_time = main
        .and_then(|value| value.get("date"))
        .and_then(Value::as_i64)
        .unwrap_or(fallback_time);
    let mut replies: Vec<ArchiveReply> = main
        .and_then(|value| value.get("replys"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(reply_from_value)
        .collect();
    if let Some(comment_id) = comment_id.as_deref() {
        let related_replies = value
            .as_ref()
            .and_then(|value| value.get("comments"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|comment| text_at(comment, "/commentid").as_deref() == Some(comment_id))
            .filter_map(|comment| comment.get("replys"))
            .filter_map(Value::as_array)
            .flatten()
            .filter_map(reply_from_value);
        for reply in related_replies {
            let duplicate = replies.iter().any(|candidate| {
                candidate.uin == reply.uin
                    && candidate.content == reply.content
                    && candidate.created_at == reply.created_at
            });
            if !duplicate {
                replies.push(reply);
            }
        }
    }

    let is_reply_notification = main
        .and_then(|value| value.get("replynum"))
        .and_then(Value::as_i64)
        .is_some_and(|count| count > 0)
        && main_uin.is_some()
        && fallback_uin.is_some()
        && main_content.as_deref() != fallback_content.as_deref()
        && fallback_time > main_time;
    if is_reply_notification {
        if let Some(content) = fallback_content.clone() {
            let duplicate = replies
                .iter()
                .any(|reply| reply.uin == fallback_uin && reply.content == content);
            if !duplicate {
                let reply_target = replies
                    .iter()
                    .filter(|reply| reply.uin != fallback_uin && reply.created_at <= fallback_time)
                    .max_by_key(|reply| reply.created_at);
                replies.push(ArchiveReply {
                    uin: fallback_uin.clone(),
                    nickname: fallback_name.clone(),
                    reply_to_uin: reply_target
                        .and_then(|reply| reply.uin.clone())
                        .or_else(|| main_uin.clone()),
                    reply_to_nickname: reply_target
                        .and_then(|reply| reply.nickname.clone())
                        .or_else(|| main_name.clone()),
                    content,
                    created_at: fallback_time,
                });
            }
        }
    }

    ArchiveComment {
        comment_id,
        uin: main_uin.or(fallback_uin),
        nickname: main_name.or(fallback_name),
        content: main_content
            .or(fallback_content)
            .unwrap_or_else(|| "评论了这条动态".into()),
        created_at: main_time,
        replies,
    }
}

pub fn reply_from_value(value: &Value) -> Option<ArchiveReply> {
    let content = text_at(value, "/content")?;
    Some(ArchiveReply {
        uin: text_at(value, "/user/uin").or_else(|| text_at(value, "/replyuser/uin")),
        nickname: text_at(value, "/user/nickname")
            .or_else(|| text_at(value, "/replyuser/nickname")),
        reply_to_uin: text_at(value, "/replyuser/uin")
            .or_else(|| text_at(value, "/targetuser/uin"))
            .or_else(|| text_at(value, "/target/uin")),
        reply_to_nickname: text_at(value, "/replyuser/nickname")
            .or_else(|| text_at(value, "/targetuser/nickname"))
            .or_else(|| text_at(value, "/target/nickname")),
        content,
        created_at: value.get("date").and_then(Value::as_i64).unwrap_or(0),
    })
}

pub fn merge_comments(comments: impl IntoIterator<Item = ArchiveComment>) -> Vec<ArchiveComment> {
    let mut merged: Vec<ArchiveComment> = Vec::new();
    for mut comment in comments {
        let existing = merged.iter_mut().find(|candidate| {
            (comment.comment_id.is_some() && candidate.comment_id == comment.comment_id)
                || (candidate.uin == comment.uin
                    && candidate.content == comment.content
                    && candidate.created_at == comment.created_at)
        });
        if let Some(existing) = existing {
            for reply in comment.replies.drain(..) {
                let duplicate = existing.replies.iter().any(|candidate| {
                    candidate.uin == reply.uin
                        && candidate.content == reply.content
                        && candidate.created_at == reply.created_at
                });
                if !duplicate {
                    existing.replies.push(reply);
                }
            }
            existing.replies.sort_by_key(|reply| reply.created_at);
        } else {
            comment.replies.sort_by_key(|reply| reply.created_at);
            merged.push(comment);
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::{comment_from_values, content_fingerprint, picture_urls, video_urls};
    use serde_json::json;

    #[test]
    fn fingerprint_is_stable_and_sensitive_to_content() {
        let first = content_fingerprint(Some("1"), 100, Some("你好"), None, None);
        let second = content_fingerprint(Some("1"), 100, Some("你好"), None, None);
        let changed = content_fingerprint(Some("1"), 100, Some("你好呀"), None, None);
        assert_eq!(first, second);
        assert_ne!(first, changed);
    }

    #[test]
    fn fingerprint_includes_media_urls() {
        let json = json!({"picdata":{"pic":[{"photourl":[{"url":"https://a.example/1.jpg"}]}]}});
        let with_pic =
            content_fingerprint(Some("1"), 100, Some("hi"), Some(&json.to_string()), None);
        let without_pic = content_fingerprint(Some("1"), 100, Some("hi"), None, None);
        assert_ne!(with_pic, without_pic);
    }

    #[test]
    fn extracts_picture_and_video_urls() {
        let pics = json!({"picdata":{"pic":[{"photourl":[{"url":"//a.example/1.jpg"}]},{"photourl":{"2":{"url":"https://a.example/2.png"}}}]}});
        let urls = picture_urls(Some(pics.to_string()));
        assert_eq!(
            urls,
            vec!["https://a.example/1.jpg", "https://a.example/2.png"]
        );

        let video = json!({"videourl":"https://v.example/a.mp4","videourls":{"fhd":{"url":"https://v.example/fhd.mp4"}}});
        let urls = video_urls(Some(video.to_string()));
        assert_eq!(
            urls,
            vec!["https://v.example/a.mp4", "https://v.example/fhd.mp4"]
        );
    }

    #[test]
    fn parses_comment_with_reply_and_merges_duplicates() {
        let json = json!({
            "main_comment": {
                "commentid": "c1",
                "content": "第一条",
                "date": 100_i64,
                "user": { "uin": "2", "nickname": "乙" },
                "replys": [{ "content": "回", "date": 110_i64, "user": { "uin": "1", "nickname": "甲" } }]
            }
        });
        let comment = comment_from_values(Some(json.to_string()), None, None, None, 0);
        assert_eq!(comment.content, "第一条");
        assert_eq!(comment.replies.len(), 1);
        let merged = super::merge_comments(vec![
            comment_from_values(Some(json.to_string()), None, None, None, 0),
            comment_from_values(Some(json.to_string()), None, None, None, 0),
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].replies.len(), 1);
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use serde_json::json;

    fn temp_connection(name: &str) -> rusqlite::Connection {
        let path = std::env::temp_dir().join(format!(
            "qza-model-{}-{}-{}.sqlite3",
            name,
            std::process::id(),
            crate::util::now_millis()
        ));
        let _ = std::fs::remove_file(&path);
        crate::db::open_database_at(&path).unwrap()
    }

    fn sample_comment_feed() -> Value {
        json!({
            "comm": {"feedskey": "311_2_key", "subid": 2, "time": 1751637966},
            "original": {
                "cell_id": {"cellid": "mood-1"},
                "cell_comm": {"time": 1751600000, "appid": 1, "feedskey": "1_1"},
                "cell_summary": {"summary": "第一条动态"},
                "cell_userinfo": {"user": {"uin": "10001", "nickname": "我"}},
                "cell_comment": {
                    "main_comment": {
                        "commentid": "c-1",
                        "content": "很棒！",
                        "date": 1751637000,
                        "user": {"uin": "10002", "nickname": "朋友"}
                    }
                }
            },
            "summary": {"summary": "评论了动态"},
            "userinfo": {"user": {"uin": "10002", "nickname": "朋友"}}
        })
    }

    #[test]
    fn save_dynamic_writes_unified_rows() {
        let mut connection = temp_connection("unified");
        let transaction = connection.transaction().unwrap();
        let feed = sample_comment_feed();
        let id = save_dynamic(&transaction, "10001", &feed, Some(7), "feeds")
            .unwrap()
            .expect("应返回动态 id");
        transaction.commit().unwrap();

        // archive_dynamics 扩展列
        let source: String = connection
            .query_row(
                "SELECT source FROM archive_dynamics WHERE id=?1",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(source, "feeds");
        let fingerprint: String = connection
            .query_row(
                "SELECT content_fingerprint FROM archive_dynamics WHERE id=?1",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!fingerprint.is_empty());

        // dynamic_sources
        let (matched, raw_id): (String, Option<i64>) = connection
            .query_row(
                "SELECT matched_by, raw_response_id FROM dynamic_sources WHERE dynamic_id=?1 AND source='feeds'",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(matched, "new");
        assert_eq!(raw_id, Some(7));

        // users
        let nickname: String = connection
            .query_row("SELECT nickname FROM users WHERE uin='10002'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(nickname, "朋友");

        // comments
        let (comment_id, content, uin): (Option<String>, String, Option<String>) = connection
            .query_row(
                "SELECT comment_id, content, uin FROM comments WHERE dynamic_id=?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(comment_id.as_deref(), Some("c-1"));
        assert_eq!(content, "很棒！");
        assert_eq!(uin.as_deref(), Some("10002"));

        // 再次保存同动态 → matched_by 变为 platform_id 且不新增动态
        let transaction2 = connection.transaction().unwrap();
        save_dynamic(&transaction2, "10001", &feed, Some(8), "feeds").unwrap();
        transaction2.commit().unwrap();
        let matched: String = connection
            .query_row(
                "SELECT matched_by FROM dynamic_sources WHERE dynamic_id=?1 AND source='feeds'",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(matched, "platform_id");
        let dynamic_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM archive_dynamics", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(dynamic_count, 1, "重复保存不应新增动态行");
    }

    #[test]
    fn saves_like_rows_from_like_events() {
        let mut connection = temp_connection("likes");
        let feed = json!({
            "comm": {"feedskey": "217_3_key", "subid": 217, "time": 1752553379},
            "original": {
                "cell_id": {"cellid": "mood-2"},
                "cell_comm": {"time": 1752500000, "appid": 1},
                "cell_summary": {"summary": "被赞的动态"},
                "cell_userinfo": {"user": {"uin": "10001", "nickname": "我"}}
            },
            "title": {"title": "赞了我"},
            "userinfo": {"user": {"uin": "10002", "nickname": "朋友"}}
        });
        let transaction = connection.transaction().unwrap();
        let id = save_dynamic(&transaction, "10001", &feed, None, "feeds")
            .unwrap()
            .unwrap();
        transaction.commit().unwrap();
        let like_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM likes WHERE dynamic_id=?1",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(like_count, 1);
    }

    #[test]
    fn guestbook_feed_writes_guestbook_entries() {
        let mut connection = temp_connection("guestbook");
        let feed = json!({
            "comm": {"feedskey": "334_1_key", "subid": 1, "time": 1752553379},
            "original": {
                "cell_id": {"cellid": "gb-1"},
                "cell_comm": {"time": 1752500000, "appid": 334}
            },
            "summary": {"summary": "来踩踩"},
            "userinfo": {"user": {"uin": "10002", "nickname": "访客"}}
        });
        let transaction = connection.transaction().unwrap();
        let id = save_dynamic(&transaction, "10001", &feed, None, "feeds")
            .unwrap()
            .unwrap();
        transaction.commit().unwrap();
        let category: String = connection
            .query_row(
                "SELECT category FROM archive_dynamics WHERE id=?1",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(category, "guestbook");
        let entry_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM guestbook_entries WHERE owner_uin='10001'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(entry_count, 1);
    }
}
