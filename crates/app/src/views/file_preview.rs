//! In-app preview of a library file: pictures, PDFs, Office documents as PDF pages, and plain text.

use std::sync::Arc;

use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_syntax::Pdf;
use hayro::vello_cpu::color::palette::css::WHITE;
use hayro::{PixmapSettings, RenderCache, RenderSettings, render};
use image::{Frame, RgbaImage};

use crate::theme;

/// Width PDF pages are rasterized at; the view scales them down to fit.
const PAGE_PIXELS: f32 = 1400.;
/// ponytail: renders up to this many pages up front; render lazily per visible page if long PDFs matter.
const MAX_PAGES: usize = 40;
const MAX_TEXT_BYTES: usize = 512 * 1024;
const PAGE_DISPLAY_WIDTH: f32 = 900.;
const PAGE_GAP: f32 = 16.;
/// Larger files open in the browser instead of being loaded into memory.
pub const MAX_PREVIEW_BYTES: u64 = 60 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewKind {
    Image,
    Pdf,
    /// Converted to PDF by SharePoint first.
    Office,
    Text,
    /// Audio and video, which play in the system player.
    Media,
}

pub fn preview_kind(name: &str) -> Option<PreviewKind> {
    let extension = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => PreviewKind::Image,
        "pdf" => PreviewKind::Pdf,
        "doc" | "docx" | "dot" | "dotx" | "xls" | "xlsx" | "xlsm" | "ppt" | "pptx" | "pps"
        | "ppsx" | "odt" | "ods" | "odp" | "rtf" => PreviewKind::Office,
        "txt" | "md" | "csv" | "tsv" | "log" | "json" | "xml" | "yaml" | "yml" | "toml" | "ini"
        | "cfg" | "conf" | "sh" | "ps1" | "py" | "rs" | "js" | "ts" | "html" | "css" | "sql" => {
            PreviewKind::Text
        }
        "mp4" | "mov" | "m4v" | "webm" | "mkv" | "avi" | "mp3" | "m4a" | "wav" | "ogg" | "flac" => {
            PreviewKind::Media
        }
        _ => return None,
    })
}

/// What a preview shows once the file is decoded, built off the UI thread.
pub enum Decoded {
    /// Picture or document pages, with the document's full page count.
    Pages(Vec<RgbaImage>, usize),
    Text(String),
}

pub fn decode(kind: PreviewKind, bytes: Vec<u8>) -> Result<Decoded, String> {
    match kind {
        PreviewKind::Image => {
            let picture = image::load_from_memory(&bytes).map_err(|error| error.to_string())?;
            Ok(Decoded::Pages(vec![picture.to_rgba8()], 1))
        }
        PreviewKind::Pdf | PreviewKind::Office => pdf_pages(bytes),
        PreviewKind::Text => Ok(Decoded::Text(text(&bytes))),
        PreviewKind::Media => Err("plays in the system player".to_owned()),
    }
}

pub fn text(bytes: &[u8]) -> String {
    let end = bytes.len().min(MAX_TEXT_BYTES);
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn pdf_pages(bytes: Vec<u8>) -> Result<Decoded, String> {
    let pdf = Pdf::new(bytes).map_err(|error| format!("{error:?}"))?;
    let pages = pdf.pages();
    let cache = RenderCache::new();
    let (interpreter, settings) = (InterpreterSettings::default(), RenderSettings::default());
    let rendered = pages
        .iter()
        .take(MAX_PAGES)
        .filter_map(|page| {
            let (width, _) = page.render_dimensions();
            let scale = PAGE_PIXELS / width.max(1.);
            let pixmap = render(
                page,
                &cache,
                &interpreter,
                &settings,
                &PixmapSettings {
                    x_scale: scale,
                    y_scale: scale,
                    bg_color: WHITE,
                },
            );
            // Premultiplied RGBA equals straight RGBA here, since the white background makes every pixel opaque.
            RgbaImage::from_raw(
                u32::from(pixmap.width()),
                u32::from(pixmap.height()),
                pixmap.data_as_u8_slice().to_vec(),
            )
        })
        .collect();
    Ok(Decoded::Pages(rendered, pages.len()))
}

/// gpui draws BGRA frames.
pub fn render_image(mut pixels: RgbaImage) -> Arc<RenderImage> {
    for pixel in pixels.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Arc::new(RenderImage::new(vec![Frame::new(pixels)]))
}

pub fn pages_view(pages: &[Arc<RenderImage>], total: usize, more_label: String) -> AnyElement {
    v_flex()
        .id("file-preview-pages")
        .size_full()
        .min_h_0()
        .items_center()
        .py(px(PAGE_GAP))
        .gap(px(PAGE_GAP))
        .children(pages.iter().map(|page| {
            let size = page.size(0);
            let (width, height) = (size.width.0 as f32, size.height.0 as f32);
            let shown = width.min(PAGE_DISPLAY_WIDTH);
            img(ImageSource::Render(page.clone()))
                .flex_none()
                .w(px(shown))
                .h(px(shown * height / width.max(1.)))
                .object_fit(ObjectFit::Contain)
                .shadow_md()
        }))
        .when(total > pages.len(), |view| {
            view.child(
                div()
                    .text_size(px(12.5))
                    .text_color(theme::text_muted())
                    .child(more_label),
            )
        })
        .overflow_y_scrollbar()
        .into_any_element()
}

pub fn text_view(content: &str, cx: &App) -> AnyElement {
    div()
        .id("file-preview-text")
        .size_full()
        .min_h_0()
        .px(px(24.))
        .py(px(16.))
        .font_family(cx.theme().mono_font_family.clone())
        .text_size(px(12.5))
        .text_color(theme::text())
        .whitespace_normal()
        .children(
            content
                .lines()
                .map(|line| div().min_h(px(17.)).child(line.to_owned())),
        )
        .overflow_y_scrollbar()
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::{Decoded, PreviewKind, decode, preview_kind, text};

    #[test]
    fn kinds_follow_the_extension_case_insensitively() {
        assert_eq!(preview_kind("Plan.PDF"), Some(PreviewKind::Pdf));
        assert_eq!(preview_kind("a.docx"), Some(PreviewKind::Office));
        assert_eq!(preview_kind("photo.JPG"), Some(PreviewKind::Image));
        assert_eq!(preview_kind("notes.md"), Some(PreviewKind::Text));
        assert_eq!(preview_kind("clip.mp4"), Some(PreviewKind::Media));
        assert_eq!(preview_kind("archive.zip"), None);
        assert_eq!(preview_kind("README"), None);
    }

    #[test]
    fn text_is_cut_and_survives_invalid_utf8() {
        assert_eq!(text(b"a\xffb"), "a\u{fffd}b");
        assert_eq!(text(&vec![b'x'; 600 * 1024]).len(), 512 * 1024);
    }

    #[test]
    fn a_picture_decodes_to_one_page() {
        let mut png = Vec::new();
        image::RgbaImage::new(3, 2)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        match decode(PreviewKind::Image, png).unwrap() {
            Decoded::Pages(pages, total) => {
                assert_eq!((pages.len(), total), (1, 1));
                assert_eq!(pages[0].dimensions(), (3, 2));
            }
            Decoded::Text(_) => panic!("a picture is no text"),
        }
    }

    /// A 200x100 pt page with a black square in its middle, xref offsets computed so the file is valid.
    fn square_pdf() -> Vec<u8> {
        let content = "0 0 0 rg 80 30 40 40 re f";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Contents 4 0 R >>".to_owned(),
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream",
                content.len()
            ),
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, object) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend(format!("{} 0 obj\n{object}\nendobj\n", index + 1).bytes());
        }
        let xref = pdf.len();
        pdf.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).bytes());
        for offset in offsets {
            pdf.extend(format!("{offset:010} 00000 n \n").bytes());
        }
        pdf.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .bytes(),
        );
        pdf
    }

    #[test]
    fn a_pdf_page_renders_at_preview_width_with_its_content() {
        let Decoded::Pages(pages, total) = decode(PreviewKind::Pdf, square_pdf()).unwrap() else {
            panic!("a PDF renders pages");
        };
        assert_eq!((pages.len(), total), (1, 1));
        let page = &pages[0];
        assert_eq!(page.dimensions(), (1400, 700));
        assert_eq!(page.get_pixel(700, 350).0, [0, 0, 0, 255]);
        assert_eq!(page.get_pixel(20, 20).0, [255, 255, 255, 255]);
    }

    #[test]
    fn a_broken_pdf_is_an_error_not_a_panic() {
        assert!(decode(PreviewKind::Pdf, b"not a pdf".to_vec()).is_err());
    }
}
