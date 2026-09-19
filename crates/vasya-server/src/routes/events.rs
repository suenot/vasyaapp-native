//! Realtime event stream over SSE.
//!
//! This is the minimal HTTP face of the in-process event bus (the bus
//! itself is `ServerContext::events`; GraphQL subscriptions in task #5
//! consume the same bus). Events are filtered to accounts the caller
//! owns; `?account=<id>` narrows to one account.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::Extension;
use serde::Deserialize;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

use crate::agent_keys::AgentIdentity;
use crate::auth::{UserId, LOCAL_USER_ID};
use crate::context::ServerContext;
use crate::error::ApiError;

#[derive(Deserialize)]
pub struct EventsQuery {
    pub account: Option<String>,
}

pub async fn sse_events(
    State(ctx): State<Arc<ServerContext>>,
    user: Extension<UserId>,
    agent: Option<Extension<AgentIdentity>>,
    Query(filter): Query<EventsQuery>,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, ApiError> {
    let user_id = user.0 .0.clone();
    let agent = agent.map(|Extension(a)| a);

    // The path-based allowlist gate in policy.rs only matches /accounts/{acc}/…,
    // so `/events` slips past it; enforce the agent's per-account allowlist here
    // (mirroring GraphQL subscriptions and the STT body handler). Reject an
    // explicit ?account= the key isn't allowed to reach.
    if let (Some(agent), Some(want)) = (&agent, &filter.account) {
        if !agent.allows_account(want) {
            return Err(ApiError::Forbidden("account not in key allowlist".into()));
        }
    }

    let rx = ctx.events.subscribe();

    let stream = BroadcastStream::new(rx).filter_map(move |item| {
        let event = match item {
            Ok(event) => event,
            // Ask consumers to resynchronize after an event gap. No account data is exposed.
            Err(_) => return Some(Ok(SseEvent::default().event("native:resync").data("{}"))),
        };

        let account = event
            .payload
            .get("accountId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Per-user isolation: only events for accounts the caller owns, and —
        // for agent keys — only accounts within the key's allowlist (an agent
        // with no ?account filter must still not receive excluded accounts).
        // Events without accountId go to the embedded local user only.
        let allowed = match &account {
            Some(acc) => {
                ctx.accounts.is_owner(&user_id, acc)
                    && agent.as_ref().map_or(true, |a| a.allows_account(acc))
            }
            None => user_id == LOCAL_USER_ID,
        };
        if !allowed {
            return None;
        }

        if let Some(want) = &filter.account {
            if account.as_deref() != Some(want.as_str()) {
                return None;
            }
        }

        let sse = SseEvent::default()
            .event(event.name)
            .json_data(event.payload)
            .ok()?;
        Some(Ok(sse))
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
