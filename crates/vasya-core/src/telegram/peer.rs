//! Shared peer resolution logic.
//!
//! Resolves a chat_id to a Peer using the wrapper's peer cache first (O(1)),
//! falling back to dialog iteration (O(n)) with progressive caching. Lives
//! in vasya-core so the desktop commands and the server routes share one
//! implementation. Errors are plain strings (engine convention); callers map
//! them onto their own transport error types.

use grammers_session::defs::PeerRef;

use super::client_manager::TelegramClientWrapper;

/// Resolve a chat_id to a Peer, using the wrapper's cache first.
pub async fn resolve_peer(
    wrapper: &TelegramClientWrapper,
    chat_id: i64,
) -> Result<grammers_client::types::Peer, String> {
    // Check cache first (O(1))
    {
        let peers = wrapper.peers.read().await;
        if let Some(peer) = peers.get(&chat_id) {
            return Ok(peer.clone());
        }
    }

    // Fallback: iterate dialogs, collecting peers for batch insert
    tracing::warn!(chat_id = chat_id, "Peer not in cache, iterating dialogs");
    let mut dialogs = wrapper.client.iter_dialogs();
    let mut found: Option<grammers_client::types::Peer> = None;
    let mut new_peers = Vec::new();

    while let Some(dialog) = dialogs
        .next()
        .await
        .map_err(|e| format!("Failed to iterate dialogs: {}", e))?
    {
        let peer = &dialog.peer;
        let id = PeerRef::from(peer).id.bot_api_dialog_id();
        new_peers.push((id, peer.clone()));

        if id == chat_id {
            found = Some(peer.clone());
            break;
        }
    }

    // Batch insert all discovered peers
    if !new_peers.is_empty() {
        let mut peers = wrapper.peers.write().await;
        for (id, peer) in new_peers {
            peers.insert(id, peer);
        }
    }

    found.ok_or_else(|| format!("Chat {} not found", chat_id))
}
