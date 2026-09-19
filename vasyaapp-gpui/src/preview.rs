//! Decode Telegram previews without GPUI's process-wide resource cache.
use gpui_kit::RenderImage;
use image::{DynamicImage, ImageDecoder, ImageReader, Limits};
use std::{path::Path, sync::Arc};

pub const MAX_ENTRIES: usize = 32;
pub const MAX_WORKERS: usize = 2;
pub const MAX_EDGE: u32 = 512;
const MAX_ENCODED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DECODED_BYTES: u64 = 64 * 1024 * 1024;

pub enum Preview {
    Loading,
    Ready(Arc<RenderImage>),
    Failed(String),
}
pub struct Entry {
    pub image: Preview,
    pub last_used: u64,
}

/// Runs only on the background executor. A decoded preview is at most 1 MiB;
/// 32 retained entries therefore use at most 32 MiB of BGRA pixel buffers.
pub fn decode(path: &Path) -> anyhow::Result<Arc<RenderImage>> {
    anyhow::ensure!(
        std::fs::metadata(path)?.len() <= MAX_ENCODED_BYTES,
        "Image file exceeds preview limit"
    );
    let mut reader = ImageReader::open(path)?.with_guessed_format()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    reader.limits(limits);
    let mut decoder = reader.into_decoder()?;
    let (width, height) = decoder.dimensions();
    anyhow::ensure!(
        u64::from(width) * u64::from(height) * 4 <= MAX_DECODED_BYTES,
        "Image dimensions exceed preview limit"
    );
    anyhow::ensure!(
        decoder.total_bytes() <= MAX_DECODED_BYTES,
        "Decoded image exceeds preview limit"
    );
    let orientation = decoder.orientation()?;
    let mut decoded = DynamicImage::from_decoder(decoder)?;
    decoded.apply_orientation(orientation);
    let mut pixels = decoded.thumbnail(MAX_EDGE, MAX_EDGE).into_rgba8();
    // GPUI's RenderImage stores BGRA rather than RGBA bytes.
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new(vec![image::Frame::new(pixels)])))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scales_preview_and_converts_to_bgra() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        image::RgbaImage::from_pixel(1024, 512, image::Rgba([200, 20, 10, 255]))
            .save(&path)
            .unwrap();
        let image = decode(&path).unwrap();
        let bytes = image.as_bytes(0).unwrap();
        assert_eq!(bytes.len(), 512 * 256 * 4);
        assert_eq!(&bytes[..4], &[10, 20, 200, 255]);
    }
    #[test]
    fn rejects_oversized_files_dimensions_and_corrupt_input() {
        let dir = tempfile::tempdir().unwrap();
        let huge = dir.path().join("huge.png");
        std::fs::File::create(&huge)
            .unwrap()
            .set_len(MAX_ENCODED_BYTES + 1)
            .unwrap();
        assert!(decode(&huge).is_err());
        let wide = dir.path().join("wide.png");
        image::RgbaImage::new(8193, 1).save(&wide).unwrap();
        assert!(decode(&wide).is_err());
        let invalid = dir.path().join("invalid.png");
        std::fs::write(&invalid, b"not an image").unwrap();
        assert!(decode(&invalid).is_err());
    }
}

/// Markdown images are explicit links: only downloaded Telegram attachments
/// enter the bounded preview pipeline. This also prevents data-URL image nodes
/// and raw HTML image tags from bypassing the cache through TextView.
pub struct MarkdownImageLinks {
    pub block: bool,
}
impl gpui_kit::component::text::MarkdownPlugin for MarkdownImageLinks {
    fn name(&self) -> &str {
        if self.block {
            "vasya-raw-markup"
        } else {
            "vasya-image-link"
        }
    }
    fn is_block(&self) -> bool {
        self.block
    }
    fn parse(
        &self,
        node: &gpui_kit::component::text::markdown_ast::Node,
        cx: &gpui_kit::component::text::MarkdownParseContext<'_>,
    ) -> Option<gpui_kit::component::text::MarkdownNode> {
        use gpui_kit::component::text::{markdown_ast::Node, MarkdownNode};
        let (label, url) = match node {
            Node::Image(image) if !self.block => (
                if image.alt.is_empty() {
                    "Open image".into()
                } else {
                    image.alt.clone()
                },
                Some(image.url.clone()),
            ),
            Node::ImageReference(image) if !self.block => (format!("Image: {}", image.alt), None),
            Node::Html(html) => (html.value.clone(), None),
            _ => return None,
        };
        Some(
            MarkdownNode::new(self.name(), url)
                .text(label)
                .markdown(cx.node_source(node).unwrap_or_default().to_string()),
        )
    }
    fn render(
        &self,
        node: &gpui_kit::component::text::MarkdownNode,
        _: &mut gpui_kit::Window,
        _: &mut gpui_kit::App,
    ) -> impl gpui_kit::IntoElement {
        use gpui_kit::{IntoElement, ParentElement};
        if let Some(Some(url)) = node.data::<Option<String>>() {
            gpui_kit::component::link::Link::new((
                "image-link",
                node.source_range().map(|r| r.start).unwrap_or_default(),
            ))
            .href(url.clone())
            .child(node.as_text().to_string())
            .into_any_element()
        } else {
            gpui_kit::div()
                .child(node.as_text().to_string())
                .into_any_element()
        }
    }
}
