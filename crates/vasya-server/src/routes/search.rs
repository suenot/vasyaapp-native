//! Global search endpoints (parity with commands/search.rs).

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::{Extension, Json};
use grammers_client::types::{Peer, User};
use grammers_session::defs::{PeerId, PeerRef};
use grammers_tl_types as tl;
use serde::Deserialize;
use std::collections::HashMap;

use crate::auth::UserId;
use crate::context::ServerContext;
use crate::dto::{GlobalMessageResult, GlobalSearchResult};
use crate::error::ApiError;
use crate::routes::account_client;

#[derive(Deserialize)]
pub struct SearchQuery {
    pub q: String,
    pub limit: Option<i32>,
}

fn full_name(first: Option<&str>, last: Option<&str>) -> String {
    let first = first.unwrap_or("");
    let last = last.unwrap_or("");
    if last.is_empty() {
        first.to_string()
    } else {
        format!("{} {}", first, last)
    }
}

/// Global search for users and channels via contacts.Search.
pub(crate) async fn global_search_op(
    ctx: &Arc<ServerContext>,
    user: &UserId,
    account_id: &str,
    q: &str,
    limit: Option<i32>,
) -> Result<Vec<GlobalSearchResult>, ApiError> {
    if q.trim().is_empty() {
        return Ok(Vec::new());
    }

    let wrapper = account_client(ctx, user, account_id).await?;
    let limit = limit.unwrap_or(20);

    let request = tl::functions::contacts::Search {
        q: q.to_string(),
        limit,
    };
    let result = wrapper
        .client
        .invoke(&request)
        .await
        .map_err(|e| ApiError::telegram(format!("Failed to perform global search: {e}")))?;

    let mut results = Vec::new();
    match result {
        tl::enums::contacts::Found::Found(found) => {
            cache_returned_peers(
                &mut *wrapper.peers.write().await,
                &found.users,
                &found.chats,
            );
            for user in &found.users {
                if let tl::enums::User::User(u) = user {
                    results.push(GlobalSearchResult {
                        id: PeerId::user(u.id).bot_api_dialog_id(),
                        title: full_name(u.first_name.as_deref(), u.last_name.as_deref()),
                        username: u.username.clone(),
                        result_type: "user".to_string(),
                        subscribers_count: None,
                    });
                }
            }

            for chat in &found.chats {
                match chat {
                    tl::enums::Chat::Channel(ch) => {
                        let result_type = if ch.broadcast { "channel" } else { "group" };
                        results.push(GlobalSearchResult {
                            id: PeerId::channel(ch.id).bot_api_dialog_id(),
                            title: ch.title.clone(),
                            username: ch.username.clone(),
                            result_type: result_type.to_string(),
                            subscribers_count: ch.participants_count,
                        });
                    }
                    tl::enums::Chat::Chat(ch) => {
                        results.push(GlobalSearchResult {
                            id: PeerId::chat(ch.id).bot_api_dialog_id(),
                            title: ch.title.clone(),
                            username: None,
                            result_type: "group".to_string(),
                            subscribers_count: Some(ch.participants_count),
                        });
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(results)
}

pub async fn global_search(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path(account_id): Path<String>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Vec<GlobalSearchResult>>, ApiError> {
    Ok(Json(
        global_search_op(&ctx, &user.0, &account_id, &query.q, query.limit).await?,
    ))
}

/// Search messages across all chats via messages.SearchGlobal.
pub(crate) async fn search_all_messages_op(
    ctx: &Arc<ServerContext>,
    user: &UserId,
    account_id: &str,
    q: &str,
    limit: Option<i32>,
) -> Result<Vec<GlobalMessageResult>, ApiError> {
    if q.trim().is_empty() {
        return Ok(Vec::new());
    }

    let wrapper = account_client(ctx, user, account_id).await?;
    let limit = limit.unwrap_or(20);

    let request = tl::functions::messages::SearchGlobal {
        broadcasts_only: false,
        groups_only: false,
        users_only: false,
        folder_id: None,
        q: q.to_string(),
        filter: tl::enums::MessagesFilter::InputMessagesFilterEmpty,
        min_date: 0,
        max_date: 0,
        offset_rate: 0,
        offset_peer: tl::enums::InputPeer::Empty,
        offset_id: 0,
        limit,
    };

    let result = wrapper
        .client
        .invoke(&request)
        .await
        .map_err(|e| ApiError::telegram(format!("Failed to search global messages: {e}")))?;

    let (raw_messages, raw_chats, raw_users) = match result {
        tl::enums::messages::Messages::Messages(m) => (m.messages, m.chats, m.users),
        tl::enums::messages::Messages::Slice(m) => (m.messages, m.chats, m.users),
        tl::enums::messages::Messages::ChannelMessages(m) => (m.messages, m.chats, m.users),
        tl::enums::messages::Messages::NotModified(_) => (Vec::new(), Vec::new(), Vec::new()),
    };

    cache_returned_peers(&mut *wrapper.peers.write().await, &raw_users, &raw_chats);
    let titles = peer_titles(&raw_users, &raw_chats);

    let mut results = Vec::new();
    for msg in raw_messages {
        if let tl::enums::Message::Message(m) = msg {
            let chat_id = canonical_peer_id(&m.peer_id);
            let chat_title = titles
                .get(&chat_id)
                .cloned()
                .unwrap_or_else(|| format!("Chat {}", chat_id));
            let sender_name = m
                .from_id
                .as_ref()
                .and_then(|peer| titles.get(&canonical_peer_id(peer)).cloned());

            // Truncate text for preview
            let text = if m.message.is_empty() {
                None
            } else if m.message.chars().count() > 200 {
                let truncated: String = m.message.chars().take(200).collect();
                Some(format!("{}...", truncated))
            } else {
                Some(m.message.clone())
            };

            results.push(GlobalMessageResult {
                message_id: m.id,
                chat_id,
                chat_title,
                sender_name,
                text,
                date: m.date as i64,
            });
        }
    }

    Ok(results)
}

pub async fn search_all_messages(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path(account_id): Path<String>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Vec<GlobalMessageResult>>, ApiError> {
    Ok(Json(
        search_all_messages_op(&ctx, &user.0, &account_id, &query.q, query.limit).await?,
    ))
}

/// All result identities use the same Bot API namespace as dialogs and resolve_peer.
fn canonical_peer_id(peer: &tl::enums::Peer) -> i64 {
    match peer {
        tl::enums::Peer::User(user) => PeerId::user(user.user_id),
        tl::enums::Peer::Chat(chat) => PeerId::chat(chat.chat_id),
        tl::enums::Peer::Channel(channel) => PeerId::channel(channel.channel_id),
    }
    .bot_api_dialog_id()
}
fn cache_returned_peers(
    cache: &mut HashMap<i64, Peer>,
    users: &[tl::enums::User],
    chats: &[tl::enums::Chat],
) {
    let peers = users
        .iter()
        .cloned()
        .map(|user| Peer::User(User::from_raw(user)))
        .chain(chats.iter().cloned().map(Peer::from_raw));
    for peer in peers {
        let reference = PeerRef::from(&peer);
        let id = reference.id.bot_api_dialog_id();
        // Sparse/minimal results must not discard a previously known access hash.
        if reference.auth.hash() == 0
            && cache
                .get(&id)
                .is_some_and(|old| PeerRef::from(old).auth.hash() != 0)
        {
            continue;
        }
        cache.insert(id, peer);
    }
}
fn peer_titles(users: &[tl::enums::User], chats: &[tl::enums::Chat]) -> HashMap<i64, String> {
    let mut titles = HashMap::new();
    for raw in users {
        if let tl::enums::User::User(user) = raw {
            titles.insert(
                PeerId::user(user.id).bot_api_dialog_id(),
                full_name(user.first_name.as_deref(), user.last_name.as_deref()),
            );
        }
    }
    for raw in chats {
        let peer = Peer::from_raw(raw.clone());
        if let Some(title) = peer.name() {
            titles.insert(peer.id().bot_api_dialog_id(), title.to_string());
        }
    }
    titles
}

#[cfg(test)]
mod tests {
    use super::*;
    use tl::Deserializable;

    fn user(id: i64) -> tl::enums::User {
        // Minimal TL user: two zero flag words and mandatory ID, then populate
        // the optional fields needed for search result identity/access tests.
        let mut bytes = vec![0u8; 8];
        bytes.extend_from_slice(&id.to_le_bytes());
        let mut user = tl::types::User::from_bytes(&bytes).unwrap();
        user.first_name = Some("Alice".into());
        user.last_name = Some("Example".into());
        user.access_hash = Some(123456);
        tl::enums::User::User(user)
    }
    fn chats(id: i64) -> Vec<tl::enums::Chat> {
        vec![
            tl::enums::Chat::Forbidden(tl::types::ChatForbidden {
                id,
                title: "Small group".into(),
            }),
            tl::enums::Chat::ChannelForbidden(tl::types::ChannelForbidden {
                id,
                title: "Public channel".into(),
                broadcast: true,
                megagroup: false,
                access_hash: 987654,
                until_date: None,
            }),
        ]
    }
    #[test]
    fn canonical_ids_and_titles_do_not_collide_between_peer_kinds() {
        let user_peer = tl::enums::Peer::User(tl::types::PeerUser { user_id: 42 });
        let chat_peer = tl::enums::Peer::Chat(tl::types::PeerChat { chat_id: 42 });
        let channel_peer = tl::enums::Peer::Channel(tl::types::PeerChannel { channel_id: 42 });
        assert_eq!(canonical_peer_id(&user_peer), 42);
        assert_eq!(canonical_peer_id(&chat_peer), -42);
        assert_eq!(canonical_peer_id(&channel_peer), -1_000_000_000_042);
        let names = peer_titles(&[user(42)], &chats(42));
        assert_eq!(names.len(), 3);
        assert_eq!(names[&canonical_peer_id(&user_peer)], "Alice Example");
        assert_eq!(names[&canonical_peer_id(&chat_peer)], "Small group");
        assert_eq!(names[&canonical_peer_id(&channel_peer)], "Public channel");
    }
    #[test]
    fn returned_peers_keep_access_hashes_for_opening_search_only_results() {
        let mut cache = HashMap::new();
        cache_returned_peers(&mut cache, &[user(42)], &chats(42));
        assert_eq!(cache.len(), 3);
        assert_eq!(PeerRef::from(&cache[&42]).auth.hash(), 123456);
        let channel_id = PeerId::channel(42).bot_api_dialog_id();
        assert_eq!(PeerRef::from(&cache[&channel_id]).auth.hash(), 987654);
        assert_eq!(cache[&channel_id].name(), Some("Public channel"));
        // A later empty-user result cannot erase an already known access hash.
        cache_returned_peers(
            &mut cache,
            &[tl::enums::User::Empty(tl::types::UserEmpty { id: 42 })],
            &[],
        );
        assert_eq!(PeerRef::from(&cache[&42]).auth.hash(), 123456);
    }
}
