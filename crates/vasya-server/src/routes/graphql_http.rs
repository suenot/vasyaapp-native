//! HTTP/WS plumbing for GraphQL.
//!
//! * POST /graphql — behind the same bearer middleware as REST (the
//!   middleware inserts `UserId`, forwarded into the resolver context).
//! * GET /graphql/ws — graphql-ws (and legacy graphql-transport-ws)
//!   subscriptions; browsers can't set WS headers, so auth runs on the
//!   `connection_init` payload: `{"Authorization": "Bearer <token>"}`.
//! * GET /graphql/sdl — schema SDL, public like /openapi.json.
//! * GET /graphql/playground — only when enabled in ServerOptions.

use std::sync::Arc;

use async_graphql::http::{playground_source, GraphQLPlaygroundConfig, ALL_WEBSOCKET_PROTOCOLS};
use async_graphql::Data;
use async_graphql_axum::{GraphQLProtocol, GraphQLRequest, GraphQLResponse, GraphQLWebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::{Html, IntoResponse, Response};
use axum::Extension;

use crate::agent_keys::AgentIdentity;
use crate::auth::UserId;
use crate::context::ServerContext;
use crate::graphql::VasyaSchema;

pub async fn graphql_post(
    Extension(schema): Extension<VasyaSchema>,
    Extension(user): Extension<UserId>,
    agent: Option<Extension<AgentIdentity>>,
    req: GraphQLRequest,
) -> GraphQLResponse {
    let mut request = req.into_inner().data(user);
    // Carry the agent identity (if the caller used a `vk_…` key) into the
    // resolver context; resolvers enforce scopes against it. Human sessions
    // have no identity here and skip the gate (all scopes implicit).
    if let Some(Extension(identity)) = agent {
        request = request.data(identity);
    }
    schema.execute(request).await.into()
}

pub async fn graphql_ws(
    Extension(schema): Extension<VasyaSchema>,
    State(ctx): State<Arc<ServerContext>>,
    protocol: GraphQLProtocol,
    ws: WebSocketUpgrade,
) -> Response {
    ws.protocols(ALL_WEBSOCKET_PROTOCOLS)
        .on_upgrade(move |stream| {
            GraphQLWebSocket::new(stream, schema, protocol)
                .on_connection_init(move |payload| connection_init(payload, ctx))
                .serve()
        })
}

/// Authenticate the WS connection from the connection_init payload.
///
/// Accepts both human session tokens (JWT / local) and agent `vk_…` keys;
/// for an agent key the resolved [`AgentIdentity`] is inserted alongside the
/// `UserId` so subscription resolvers can enforce scopes and the per-account
/// allowlist (mirroring the HTTP path).
async fn connection_init(
    payload: serde_json::Value,
    ctx: Arc<ServerContext>,
) -> async_graphql::Result<Data> {
    let bearer = payload
        .get("Authorization")
        .or_else(|| payload.get("authorization"))
        .and_then(|v| v.as_str())
        .map(|s| s.strip_prefix("Bearer ").unwrap_or(s))
        .ok_or_else(|| {
            async_graphql::Error::new("Missing Authorization in connection_init payload")
        })?;

    let mut data = Data::default();
    if bearer.starts_with("vk_") {
        let (user_id, identity) = ctx
            .agent_keys
            .authenticate(bearer)
            .ok_or_else(|| async_graphql::Error::new("Unauthorized"))?;
        data.insert(UserId(user_id));
        data.insert(identity);
    } else {
        let user = ctx
            .auth
            .authenticate(bearer)
            .map_err(|_| async_graphql::Error::new("Unauthorized"))?;
        data.insert(user);
    }
    Ok(data)
}

pub async fn graphql_sdl(Extension(schema): Extension<VasyaSchema>) -> Response {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        schema.sdl(),
    )
        .into_response()
}

pub async fn graphql_playground() -> Html<String> {
    Html(playground_source(
        GraphQLPlaygroundConfig::new("/api/v1/graphql").subscription_endpoint("/api/v1/graphql/ws"),
    ))
}
