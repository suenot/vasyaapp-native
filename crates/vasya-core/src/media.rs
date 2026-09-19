//! Shared media type classification
//!
//! Single source of truth for mapping `grammers_client::types::Media` variants
//! to human-readable type strings used throughout the application.

use grammers_client::types::Media;

/// Classify a media object into a human-readable type string.
/// Returns one of: "photo", "video", "audio", "voice", "document", "webpage", "other"
pub fn classify_media_type(media: &Media) -> &'static str {
    match media {
        Media::Photo(_) => "photo",
        Media::WebPage(_) => "webpage",
        Media::Document(doc) => {
            doc.mime_type()
                .map(|mime| {
                    if mime.starts_with("video/") {
                        "video"
                    } else if mime.starts_with("audio/") {
                        if mime == "audio/ogg" {
                            "voice"
                        } else {
                            "audio"
                        }
                    } else if mime.starts_with("image/") {
                        "photo" // NOT "sticker" — stickers have special attributes
                    } else {
                        "document"
                    }
                })
                .unwrap_or("document")
        }
        _ => "other",
    }
}
