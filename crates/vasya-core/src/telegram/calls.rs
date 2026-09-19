//! Telegram 1:1 voice/video call signaling (engine layer).
//!
//! Tauri-free call-control logic shared by the desktop commands
//! (`src/commands/calls.rs`) and the server REST/GraphQL routes. Only the
//! MTProto signaling and DH key exchange live here — request/accept/confirm/
//! discard plus the shared-call registry. Real-time audio capture/playback
//! (the VoIP sidecar) stays in the desktop crate: a headless server has no
//! microphone or speaker, so call *mute/volume* are not part of this module.
//!
//! Errors are plain strings (engine convention); callers map them onto their
//! own transport error types.

use std::sync::Arc;

use grammers_session::defs::PeerRef;
use grammers_tl_types as tl;
use tokio::sync::RwLock;

use super::call_state::*;
use super::client_manager::TelegramClientWrapper;
use super::dh::*;
use super::peer::resolve_peer;

/// Build the standard PhoneCallProtocol used for all call requests.
fn build_protocol() -> tl::enums::PhoneCallProtocol {
    tl::enums::PhoneCallProtocol::Protocol(tl::types::PhoneCallProtocol {
        udp_p2p: true,
        udp_reflector: true,
        min_layer: 92,
        max_layer: 92,
        library_versions: vec!["5.0.0".to_string()],
    })
}

/// Fetch or reuse the DH config from Telegram.
async fn get_dh_config(
    wrapper: &TelegramClientWrapper,
    active_calls: &Arc<RwLock<ActiveCalls>>,
) -> Result<(DhConfig, Vec<u8>), String> {
    let current_version = {
        let calls = active_calls.read().await;
        calls.dh_config.as_ref().map(|c| c.version).unwrap_or(0)
    };

    let request = tl::functions::messages::GetDhConfig {
        version: current_version,
        random_length: 256,
    };

    let result = wrapper
        .client
        .invoke(&request)
        .await
        .map_err(|e| format!("Failed to get DH config: {}", e))?;

    match result {
        tl::enums::messages::DhConfig::Config(config) => {
            let dh_config = DhConfig {
                g: config.g as u32,
                p: config.p.clone(),
                version: config.version,
            };
            let random = config.random;

            // Cache the config
            {
                let mut calls = active_calls.write().await;
                calls.dh_config = Some(dh_config.clone());
            }

            Ok((dh_config, random))
        }
        tl::enums::messages::DhConfig::NotModified(not_modified) => {
            let calls = active_calls.read().await;
            let dh_config = calls
                .dh_config
                .as_ref()
                .ok_or("DH config not cached but server returned NotModified")?
                .clone();
            Ok((dh_config, not_modified.random))
        }
    }
}

/// Extract PhoneCall info from the phone.PhoneCall response.
fn extract_phone_call(
    response: tl::enums::phone::PhoneCall,
) -> Result<tl::enums::PhoneCall, String> {
    match response {
        tl::enums::phone::PhoneCall::Call(call) => Ok(call.phone_call),
    }
}

/// Request a 1:1 (audio or video) call: fetch DH config, build the DH
/// exchange, resolve the peer, and invoke `phone.requestCall`. The resulting
/// call is stored in `active_calls`.
pub async fn request_call(
    wrapper: &TelegramClientWrapper,
    active_calls: &Arc<RwLock<ActiveCalls>>,
    account_id: &str,
    user_id: i64,
    is_video: bool,
) -> Result<CallInfoResponse, String> {
    // 1. Fetch DH config
    tracing::info!("Fetching DH config...");
    let (dh_config, server_random) = get_dh_config(wrapper, active_calls).await?;
    tracing::info!(dh_version = dh_config.version, "DH config fetched");

    // 2. Create DH exchange
    let dh_exchange = DhExchange::new(&dh_config, &server_random);
    let g_a_hash = dh_exchange.g_a_hash.clone();
    tracing::info!("DH exchange created");

    // 3. Resolve user to InputUser
    tracing::info!(user_id = user_id, "Resolving peer...");
    let peer = resolve_peer(wrapper, user_id).await?;
    let peer_ref = PeerRef::from(&peer);
    let input_user: tl::enums::InputUser = peer_ref.into();
    tracing::info!("Peer resolved");

    // 4. Generate random_id
    let random_id = rand::random::<i32>();

    // 5. Build and invoke request
    tracing::info!("Invoking phone.requestCall...");
    let request = tl::functions::phone::RequestCall {
        video: is_video,
        user_id: input_user,
        random_id,
        g_a_hash,
        protocol: build_protocol(),
    };

    let result = wrapper
        .client
        .invoke(&request)
        .await
        .map_err(|e| format!("Failed to request call: {}", e))?;
    tracing::info!("phone.requestCall response received");

    // 6. Parse response
    let phone_call = extract_phone_call(result)?;

    let (call_id, access_hash, peer_user_id, call_state) = match &phone_call {
        tl::enums::PhoneCall::Waiting(w) => {
            (w.id, w.access_hash, w.participant_id, CallState::Waiting)
        }
        tl::enums::PhoneCall::Requested(r) => {
            (r.id, r.access_hash, r.participant_id, CallState::Requesting)
        }
        other => {
            tracing::warn!("Unexpected phone call state after requestCall: {:?}", other);
            return Err("Unexpected call state in response".to_string());
        }
    };

    // 7. Store in active_calls
    let call_info = CallInfo {
        call_id,
        access_hash,
        peer_user_id,
        is_outgoing: true,
        is_video,
        state: call_state,
        dh_exchange: Some(dh_exchange),
        shared_key: None,
        key_fingerprint: None,
        account_id: account_id.to_string(),
    };

    let response = call_info.to_response();

    {
        let mut calls = active_calls.write().await;
        calls.calls.insert(call_id, call_info);
    }

    tracing::info!(call_id = call_id, "Call requested successfully");
    Ok(response)
}

/// Accept an incoming call (callee side): build the callee DH value and
/// invoke `phone.acceptCall`.
pub async fn accept_call(
    wrapper: &TelegramClientWrapper,
    active_calls: &Arc<RwLock<ActiveCalls>>,
    call_id: i64,
) -> Result<CallInfoResponse, String> {
    // 1. Get call from active_calls
    let access_hash = {
        let calls = active_calls.read().await;
        let call = calls
            .calls
            .get(&call_id)
            .ok_or("Call not found in active calls")?;
        call.access_hash
    };

    // 2. Get DH config and create exchange for callee
    let (dh_config, server_random) = get_dh_config(wrapper, active_calls).await?;
    let dh_exchange = DhExchange::new(&dh_config, &server_random);
    let g_b = dh_exchange.g_x.clone();

    // 3. Invoke phone.acceptCall
    let peer = tl::enums::InputPhoneCall::Call(tl::types::InputPhoneCall {
        id: call_id,
        access_hash,
    });

    let request = tl::functions::phone::AcceptCall {
        peer,
        g_b,
        protocol: build_protocol(),
    };

    let result = wrapper
        .client
        .invoke(&request)
        .await
        .map_err(|e| format!("Failed to accept call: {}", e))?;

    // 4. Parse response
    let _phone_call = extract_phone_call(result)?;
    let new_state = CallState::Accepted;

    // 5. Update state
    let response = {
        let mut calls = active_calls.write().await;
        if let Some(call) = calls.calls.get_mut(&call_id) {
            call.state = new_state;
            call.dh_exchange = Some(dh_exchange);
            call.to_response()
        } else {
            return Err("Call disappeared from active calls".to_string());
        }
    };

    tracing::info!(call_id = call_id, "Call accepted successfully");
    Ok(response)
}

/// Confirm a call (caller side): compute the shared key from the callee's
/// `g_b` and invoke `phone.confirmCall`.
pub async fn confirm_call(
    wrapper: &TelegramClientWrapper,
    active_calls: &Arc<RwLock<ActiveCalls>>,
    call_id: i64,
    g_b: Vec<u8>,
) -> Result<CallInfoResponse, String> {
    // 1. Get call and compute shared key
    let (access_hash, g_a, key_fingerprint) = {
        let mut calls = active_calls.write().await;
        let call = calls
            .calls
            .get_mut(&call_id)
            .ok_or("Call not found in active calls")?;

        let dh_exchange = call
            .dh_exchange
            .as_ref()
            .ok_or("DH exchange not initialized for this call")?;

        let g_a = dh_exchange.g_x.clone();

        // Compute shared key from g_b (the callee's value)
        let (shared_key, fingerprint) = dh_exchange.compute_shared_key(&g_b)?;

        call.shared_key = Some(shared_key);
        call.key_fingerprint = Some(fingerprint);
        call.state = CallState::Active;

        (call.access_hash, g_a, fingerprint)
    };

    // 2. Invoke phone.confirmCall
    let peer = tl::enums::InputPhoneCall::Call(tl::types::InputPhoneCall {
        id: call_id,
        access_hash,
    });

    let request = tl::functions::phone::ConfirmCall {
        peer,
        g_a,
        key_fingerprint,
        protocol: build_protocol(),
    };

    let result = wrapper
        .client
        .invoke(&request)
        .await
        .map_err(|e| format!("Failed to confirm call: {}", e))?;

    // 3. Parse response
    let _phone_call = extract_phone_call(result)?;

    // 4. Return current state
    let response = {
        let calls = active_calls.read().await;
        let call = calls
            .calls
            .get(&call_id)
            .ok_or("Call not found in active calls")?;
        call.to_response()
    };

    tracing::info!(call_id = call_id, "Call confirmed successfully");
    Ok(response)
}

/// Discard (hang up / reject) a call and remove it from the registry.
pub async fn discard_call(
    wrapper: &TelegramClientWrapper,
    active_calls: &Arc<RwLock<ActiveCalls>>,
    call_id: i64,
    reason: &str,
) -> Result<(), String> {
    // Get call info
    let access_hash = {
        let calls = active_calls.read().await;
        let call = calls
            .calls
            .get(&call_id)
            .ok_or("Call not found in active calls")?;
        call.access_hash
    };

    // Map reason string to PhoneCallDiscardReason enum
    let discard_reason = match reason {
        "missed" => tl::enums::PhoneCallDiscardReason::Missed,
        "disconnect" => tl::enums::PhoneCallDiscardReason::Disconnect,
        "busy" => tl::enums::PhoneCallDiscardReason::Busy,
        _ => tl::enums::PhoneCallDiscardReason::Hangup,
    };

    let peer = tl::enums::InputPhoneCall::Call(tl::types::InputPhoneCall {
        id: call_id,
        access_hash,
    });

    let request = tl::functions::phone::DiscardCall {
        video: false,
        peer,
        duration: 0,
        reason: discard_reason,
        connection_id: 0,
    };

    wrapper
        .client
        .invoke(&request)
        .await
        .map_err(|e| format!("Failed to discard call: {}", e))?;

    // Remove from active_calls
    {
        let mut calls = active_calls.write().await;
        calls.calls.remove(&call_id);
    }

    tracing::info!(call_id = call_id, "Call discarded successfully");
    Ok(())
}
