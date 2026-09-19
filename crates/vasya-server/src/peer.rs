//! Peer resolution, ported from the app's commands/peer_resolve.rs:
//! cache first (O(1)), dialog iteration fallback with progressive caching.

use grammers_session::defs::PeerRef;
use vasya_core::telegram::client_manager::TelegramClientWrapper;

use crate::error::ApiError;

pub async fn resolve_peer(
    wrapper: &TelegramClientWrapper,
    chat_id: i64,
) -> Result<grammers_client::types::Peer, ApiError> {
    {
        let peers = wrapper.peers.read().await;
        if let Some(peer) = peers.get(&chat_id) {
            return Ok(peer.clone());
        }
    }

    tracing::warn!(chat_id = chat_id, "Peer not in cache, iterating dialogs");
    let mut dialogs = wrapper.client.iter_dialogs();
    let mut found: Option<grammers_client::types::Peer> = None;
    let mut new_peers = Vec::new();

    while let Some(dialog) = dialogs
        .next()
        .await
        .map_err(|e| ApiError::telegram(format!("Failed to iterate dialogs: {e}")))?
    {
        let peer = &dialog.peer;
        let id = PeerRef::from(peer).id.bot_api_dialog_id();
        new_peers.push((id, peer.clone()));

        if id == chat_id {
            found = Some(peer.clone());
            break;
        }
    }

    if !new_peers.is_empty() {
        let mut peers = wrapper.peers.write().await;
        for (id, peer) in new_peers {
            peers.insert(id, peer);
        }
    }

    found.ok_or_else(|| ApiError::NotFound(format!("Chat {chat_id} not found")))
}
