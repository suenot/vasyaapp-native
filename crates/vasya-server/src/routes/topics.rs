//! Forum topics (parity with commands/topics.rs).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::{Extension, Json};
use grammers_session::defs::PeerRef;
use grammers_tl_types as tl;

use crate::auth::UserId;
use crate::context::ServerContext;
use crate::dto::ForumTopic;
use crate::error::ApiError;
use crate::peer::resolve_peer;
use crate::routes::account_client;

pub(crate) async fn get_forum_topics_op(
    ctx: &Arc<ServerContext>,
    user: &UserId,
    account_id: &str,
    chat_id: i64,
) -> Result<Vec<ForumTopic>, ApiError> {
    let wrapper = account_client(ctx, user, account_id).await?;
    let peer = resolve_peer(&wrapper, chat_id).await?;
    let input_peer: tl::enums::InputPeer = PeerRef::from(&peer).into();

    let request = tl::functions::messages::GetForumTopics {
        peer: input_peer,
        q: None,
        offset_date: 0,
        offset_id: 0,
        offset_topic: 0,
        limit: 100,
    };

    let result = wrapper
        .client
        .invoke(&request)
        .await
        .map_err(|e| ApiError::telegram(format!("Failed to get forum topics: {e}")))?;

    let tl::enums::messages::ForumTopics::Topics(forum_topics) = result;

    let topics: Vec<ForumTopic> = forum_topics
        .topics
        .into_iter()
        .filter_map(|topic| match topic {
            tl::enums::ForumTopic::Topic(t) => Some(ForumTopic {
                id: t.id,
                title: t.title,
                icon_color: t.icon_color,
                icon_emoji_id: t.icon_emoji_id,
                unread_count: t.unread_count,
                top_message: t.top_message,
                is_pinned: t.pinned,
                is_closed: t.closed,
            }),
            tl::enums::ForumTopic::Deleted(_) => None,
        })
        .collect();

    Ok(topics)
}

pub async fn get_forum_topics(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    Path((account_id, chat_id)): Path<(String, i64)>,
) -> Result<Json<Vec<ForumTopic>>, ApiError> {
    Ok(Json(
        get_forum_topics_op(&ctx, &user.0, &account_id, chat_id).await?,
    ))
}
