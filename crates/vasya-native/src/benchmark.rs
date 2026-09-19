//! Explicit, deterministic offline fixtures. Never enabled without `--stress-test`.
//! Every visible dataset is labelled BENCHMARK; this module performs no I/O.
use anyhow::Result;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicI32, Ordering};
use vasya_core::events::Event;

pub(crate) const CHAT_COUNT: usize = 10_000;
pub(crate) const MESSAGE_COUNT: i32 = 100_000;
const ACCOUNT: &str = "benchmark";
const DATE: i64 = 1_789_776_000;
static NEXT_SENT_ID: AtomicI32 = AtomicI32::new(10_000_000);

pub(crate) fn enabled() -> bool {
    std::env::args_os().any(|argument| argument == "--stress-test")
}

/// REST-shaped fixture response, computed only for the requested history page.
pub fn request(method: &str, path: &str, body: &Value) -> Result<Value> {
    let (route, query) = path.split_once('?').unwrap_or((path, ""));
    match (method, route) {
        ("GET", "/native/connection") => {
            return Ok(json!({"remote":false,"baseUrl":null,"benchmark":true}))
        }
        ("GET", "/native/settings") => {
            return Ok(
                json!({"dark":true,"scale":1.0,"notifications":false,"auto_transcribe":false,"benchmark":true}),
            )
        }
        ("PUT", "/native/settings") => return Ok(body.clone()),
        ("GET", "/native/cache") => return Ok(Value::Null),
        ("GET", "/api/v1/telegram/credentials") => {
            return Ok(json!({"configured":true,"source":"BENCHMARK","apiId":0}))
        }
        ("GET", "/api/v1/accounts") => {
            return Ok(
                json!([{"accountId":ACCOUNT,"phone":"BENCHMARK · Offline performance fixture"}]),
            )
        }
        ("GET", "/api/v1/health") => return Ok(json!({"status":"ok","benchmark":true})),
        ("GET", "/native/capabilities") => {
            return Ok(
                json!({"benchmark":true,"embedded":false,"remote":false,"localApi":false,"deepgram":false,"localWhisper":false,"calls":{"audio":false,"video":false}}),
            )
        }
        _ => {}
    }
    let segments: Vec<_> = route.split('/').filter(|s| !s.is_empty()).collect();
    if method == "GET" && route == "/api/v1/accounts/benchmark/chats" {
        return Ok(Value::Array((1..=CHAT_COUNT).map(chat).collect()));
    }
    if route.ends_with("/folders")
        || route.ends_with("/topics")
        || route.ends_with("/tabs")
        || route.ends_with("/search")
    {
        return Ok(json!([]));
    }
    if segments.len() == 7
        && segments[2] == "accounts"
        && segments[3] == ACCOUNT
        && segments[4] == "chats"
        && segments[6] == "messages"
    {
        let chat_id = segments[5].parse::<i64>()?;
        if method == "GET" {
            let offset = query_number(query, "offset_id").unwrap_or(0);
            let limit = query_number(query, "limit").unwrap_or(50).clamp(0, 50) as usize;
            let topic = query_number(query, "topic_id");
            let upper = if offset > 0 {
                (offset - 1).min(MESSAGE_COUNT as i64)
            } else {
                MESSAGE_COUNT as i64
            };
            return Ok(Value::Array(
                (1..=upper)
                    .rev()
                    .filter(|id| topic.is_none_or(|topic| ((*id - 1) % 10) + 1 == topic))
                    .take(limit)
                    .map(|id| message(id as i32, chat_id))
                    .collect(),
            ));
        }
        if method == "POST" {
            let id = NEXT_SENT_ID.fetch_add(1, Ordering::Relaxed);
            return Ok(
                json!({"id":id,"chat_id":chat_id,"sender_name":"You · BENCHMARK","text":body["text"].as_str().unwrap_or(""),"date":DATE,"is_outgoing":true,"media":null}),
            );
        }
    }
    // Unused benchmark-only actions have no real external effects.
    Ok(Value::Null)
}

fn query_number(query: &str, name: &str) -> Option<i64> {
    query
        .split('&')
        .filter_map(|field| field.split_once('='))
        .find_map(|(key, value)| (key == name).then(|| value.parse().ok()).flatten())
}
fn chat(id: usize) -> Value {
    json!({"id":id,"title":if id==1 { "BENCHMARK · Live stream 100 events/s".to_string() } else { format!("BENCHMARK · Conversation {id:05}") },"username":null,"unreadCount":id%17,"chatType":if id%3==0 {"group"} else {"user"},"lastMessage":format!("Offline fixture {id} · scroll to test 10,000 conversations"),"avatarPath":null,"isForum":false,"isMuted":true})
}
fn message(id: i32, chat: i64) -> Value {
    let text = match id % 7 {
        0 => format!("BENCHMARK #{id} · A longer planning update.\nThe background stream continues while you scroll, select conversations, and type.\nThis third line exercises variable-height message layout and keeps the sample readable."),
        1 => format!("BENCHMARK #{id} · Short reply."),
        2 => format!("BENCHMARK #{id} · Проверка кириллицы и переноса строк.\nAll content is generated locally; no Telegram account is connected."),
        3 => format!("BENCHMARK #{id} · Checklist\n1. Scroll the history\n2. Type while events arrive\n3. Switch conversations\n4. Confirm the input remains responsive"),
        _ => format!("BENCHMARK #{id} · Offline message in conversation {chat}. This sample contains enough text to wrap naturally at a narrow window width."),
    };
    let media = if id % 97 == 0 {
        json!([{"media_type":"document","file_name":"BENCHMARK-report.pdf","file_size":20480,"mime_type":"application/pdf"}])
    } else {
        Value::Null
    };
    json!({"id":id,"chat_id":chat,"from_user_id":id%8+1,"sender_name":format!("BENCHMARK teammate {}",id%8+1),"text":text,"date":DATE-(MESSAGE_COUNT-id) as i64*15,"is_outgoing":id%4==0,"media":media})
}

/// The actor invokes this once per 10 ms. All updates belong to chat 1.
pub fn event(sequence: u64) -> Event {
    let id = MESSAGE_COUNT as u64 + 1 + sequence;
    Event {
        name: "telegram:new-message".into(),
        payload: json!({"accountId":ACCOUNT,"chatId":1,"id":id.min(i32::MAX as u64),"senderName":"BENCHMARK live stream","text":format!("BENCHMARK live event {sequence} · synthetic 100 events/s"),"date":DATE+(sequence/100) as i64,"isOutgoing":false,"mediaType":null}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixture_has_ten_thousand_chats_with_live_chat_first() {
        let chats = request(
            "GET",
            "/api/v1/accounts/benchmark/chats?source=live",
            &Value::Null,
        )
        .unwrap();
        assert_eq!(chats.as_array().unwrap().len(), CHAT_COUNT);
        assert_eq!(chats[0]["id"], 1);
        assert!(chats[0]["title"].as_str().unwrap().contains("BENCHMARK"));
    }
    #[test]
    fn history_pages_cover_one_hundred_thousand_distinct_messages() {
        let mut offset = 0;
        let mut count = 0;
        let mut previous = MESSAGE_COUNT + 1;
        loop {
            let page = request(
                "GET",
                &format!("/api/v1/accounts/benchmark/chats/1/messages?offset_id={offset}&limit=50"),
                &Value::Null,
            )
            .unwrap();
            let page = page.as_array().unwrap();
            if page.is_empty() {
                break;
            }
            assert!(page.len() <= 50);
            for item in page {
                let id = item["id"].as_i64().unwrap() as i32;
                assert_eq!(id, previous - 1);
                previous = id;
                count += 1;
            }
            offset = previous;
        }
        assert_eq!(count, MESSAGE_COUNT);
        assert_eq!(previous, 1);
    }
    #[test]
    fn topic_pages_are_scoped_and_limit_is_bounded() {
        let page = request(
            "GET",
            "/api/v1/accounts/benchmark/chats/1/messages?topic_id=3&limit=999999",
            &Value::Null,
        )
        .unwrap();
        assert_eq!(page.as_array().unwrap().len(), 50);
        for item in page.as_array().unwrap() {
            assert_eq!((item["id"].as_i64().unwrap() - 1) % 10 + 1, 3);
        }
        assert_eq!(event(0).payload["chatId"], 1);
    }
}
