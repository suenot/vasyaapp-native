//! FLOOD_WAIT handling, ported from the app's commands/flood_wait.rs.
//!
//! Telegram returns FLOOD_WAIT errors when too many requests are made.
//! Short waits are absorbed server-side (sleep + one retry); anything that
//! still fails surfaces as HTTP 429 with Retry-After (see error.rs).

use std::future::Future;

/// Max wait time absorbed server-side before giving up and surfacing 429.
const MAX_FLOOD_WAIT_SECS: u64 = 60;

/// Parse FLOOD_WAIT seconds from an error string.
/// Matches patterns like "FLOOD_WAIT caused by ... (value: 30)"
pub fn parse_flood_wait_secs(err_str: &str) -> Option<u64> {
    if !err_str.contains("FLOOD_WAIT") {
        return None;
    }
    // Pattern: "(value: N)" at the end
    if let Some(start) = err_str.rfind("(value: ") {
        let after = &err_str[start + 8..];
        if let Some(end) = after.find(')') {
            return after[..end].parse::<u64>().ok();
        }
    }
    None
}

/// Execute an async operation with FLOOD_WAIT retry.
/// If the operation fails with FLOOD_WAIT, waits the specified duration + 1 second
/// (capped), then retries once.
pub async fn with_flood_wait_retry<F, Fut, T, E>(op: F) -> Result<T, E>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    match op().await {
        Ok(val) => Ok(val),
        Err(e) => {
            let err_str = e.to_string();
            if let Some(wait_secs) = parse_flood_wait_secs(&err_str) {
                let capped = wait_secs.min(MAX_FLOOD_WAIT_SECS);
                tracing::info!(
                    wait_secs = capped,
                    "FLOOD_WAIT detected, waiting before retry"
                );
                tokio::time::sleep(std::time::Duration::from_secs(capped + 1)).await;
                op().await
            } else {
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_flood_wait() {
        assert_eq!(
            parse_flood_wait_secs("rpc error 420: FLOOD_WAIT caused by upload.getFile (value: 2)"),
            Some(2)
        );
        assert_eq!(
            parse_flood_wait_secs(
                "rpc error 420: FLOOD_WAIT caused by photos.getUserPhotos (value: 30)"
            ),
            Some(30)
        );
        assert_eq!(parse_flood_wait_secs("some other error"), None);
    }
}
