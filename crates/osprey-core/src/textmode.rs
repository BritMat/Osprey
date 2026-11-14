//! Text-only mode. Turns a web page into just its words and links:
//! images, GIFs, video, audio, canvases, SVGs, embeds, scripts and styles
//! are all removed. Images that have a description (alt text) become a
//! small `[image: description]` link, so you can still open one if you
//! want to. Ads are dropped with the same cosmetic filters uBlock Origin
//! uses, and links to ad or tracker domains are removed too.
//!
//! The output is either a light, readable HTML page or plain text with
//! numbered link references.

use std::cell::RefCell;
use std::collections::HashSet;

use lol_html::html_content::ContentType;
use lol_html::{element, rewrite_str, RewriteStrSettings, Selector};
use url::Url;

use crate::error::{Error, Result};
use crate::filter::{Filters, RequestKind};

#[derive(Debug, Clone)]
pub struct TextModeOptions {
    /// Turn described images into `[image: ...]` links. When false they
    /// are dropped completely.
    pub image_links: bool,
    /// Show a small bar at the top saying which page this is.
    pub banner: bool,
}

impl Default for TextModeOptions {
    fn default() -> Self {
        Self {
            image_links: true,
            banner: true,
        }
    }
}

/// Everything that is removed together with its content.
const REMOVE: &str = "script, style, noscript, iframe, frame, frameset, object, embed, video, audio, \
    canvas, svg, source, track, link, template, form, button, input, select, textarea, dialog, map, \
    area, portal";

/// Attributes kept after cleaning. Everything else (style, class,
/// onclick, srcset, width and so on) goes.
const KEEP_ATTRS: &[&str] = &[
    "href", "title", "id", "name", "lang", "dir", "colspan", "rowspan", "scope", "datetime",
    "start", "reversed", "charset",
];

const STYLE: &str = r#"<meta name="viewport" content="width=device-width, initial-scale=1"><style>
:root { color-scheme: light dark; --fg: #1d1d1f; --bg: #fbfbf8; --muted: #6b6b6b; --link: #0b57d0; --rule: #ddd; }
@media (prefers-color-scheme: dark) { :root { --fg: #e6e6e3; --bg: #161616; --muted: #9a9a9a; --link: #8ab4f8; --rule: #333; } }
html { background: var(--bg); color: var(--fg); }
body { max-width: 46rem; margin: 0 auto; padding: 1.5rem 1rem 4rem; font: 17px/1.65 system-ui, -apple-system, "Segoe UI", sans-serif; overflow-wrap: anywhere; }
a { color: var(--link); }
pre, code { font-family: ui-monospace, Consolas, monospace; font-size: 0.92em; white-space: pre-wrap; }
table { border-collapse: collapse; } td, th { border: 1px solid var(--rule); padding: 0.25rem 0.5rem; }
blockquote { margin-left: 0; padding-left: 1rem; border-left: 3px solid var(--rule); color: var(--muted); }
.osprey-img { color: var(--muted); font-size: 0.9em; }
.osprey-banner { font-size: 0.85rem; color: var(--muted); border-bottom: 1px solid var(--rule); padding-bottom: 0.5rem; margin-bottom: 1.5rem; }
</style>"#;

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn absolutize(base: &Url, link: &str) -> Option<String> {
    let link = link.trim();
    if link.is_empty() || link.starts_with('#') {
        return Some(link.to_string());
    }
    let abs = base.join(link).ok()?;
    match abs.scheme() {
        "http" | "https" | "mailto" | "tel" | "file" => Some(abs.to_string()),
        _ => None,
    }
}

fn rewrite_error(e: impl std::fmt::Display) -> Error {
    Error::Parse(format!("could not process the page: {e}"))
}

/// Collects every class and id on the page so generic cosmetic filters
/// (like `##.ad-banner`) can be matched.
fn collect_classes_and_ids(html: &str) -> Result<(HashSet<String>, HashSet<String>)> {
    let classes = RefCell::new(HashSet::new());
    let ids = RefCell::new(HashSet::new());
    rewrite_str(
        html,
        RewriteStrSettings::new().append_element_content_handler(element!("*", |el| {
            if let Some(c) = el.get_attribute("class") {
                classes
                    .borrow_mut()
                    .extend(c.split_whitespace().map(str::to_string));
            }
            if let Some(i) = el.get_attribute("id") {
                ids.borrow_mut().insert(i.trim().to_string());
            }
            Ok(())
        })),
    )
    .map_err(rewrite_error)?;
    Ok((classes.into_inner(), ids.into_inner()))
}

/// Produces the text-only HTML version of `html`, which was loaded from
/// `page_url`.
pub fn simplify(
    html: &str,
    page_url: &Url,
    filters: Option<&Filters>,
    opts: &TextModeOptions,
) -> Result<String> {
    // Fragments and odd pages may lack <head> or <body>. Wrapping keeps
    // the style and banner injection working.
    let wrapped;
    let html = if html.to_ascii_lowercase().contains("<body") {
        html
    } else {
        wrapped = format!("<!doctype html><html><head></head><body>{html}</body></html>");
        &wrapped
    };

    let page = page_url.as_str();
    let mut ad_selectors: Vec<String> = Vec::new();
    if let Some(f) = filters {
        let (classes, ids) = collect_classes_and_ids(html)?;
        for s in f.hidden_selectors(page, &classes, &ids) {
            // lol_html supports most, but not all, CSS. Unsupported
            // selectors (like :has) are skipped.
            if s.parse::<Selector>().is_ok() {
                ad_selectors.push(s);
            }
        }
    }

    let mut settings = RewriteStrSettings::new();

    for sel in &ad_selectors {
        // Already validated above, so the macro's parse cannot fail.
        settings = settings.append_element_content_handler(element!(sel, |el| {
            el.remove();
            Ok(())
        }));
    }

    settings = settings
        .append_element_content_handler(element!(REMOVE, |el| {
            el.remove();
            Ok(())
        }))
        .append_element_content_handler(element!("picture", |el| {
            el.remove_and_keep_content();
            Ok(())
        }))
        .append_element_content_handler(element!("meta", |el| {
            if el.get_attribute("charset").is_none() {
                el.remove();
            }
            Ok(())
        }))
        .append_element_content_handler(element!("img, image", |el| {
            let alt = el.get_attribute("alt").unwrap_or_default();
            let alt = alt.trim();
            let src = el
                .get_attribute("src")
                .or_else(|| el.get_attribute("data-src"))
                .and_then(|s| absolutize(page_url, &s))
                .filter(|s| !s.starts_with('#'));
            if alt.is_empty() || !opts.image_links {
                el.remove();
                return Ok(());
            }
            let label = format!("[image: {}]", escape(alt));
            let replacement = match src {
                Some(s) => format!(r#"<a class="osprey-img" href="{}">{label}</a>"#, escape(&s)),
                None => format!(r#"<span class="osprey-img">{label}</span>"#),
            };
            el.replace(&replacement, ContentType::Html);
            Ok(())
        }))
        .append_element_content_handler(element!("a[href]", |el| {
            let href = el.get_attribute("href").unwrap_or_default();
            match absolutize(page_url, &href) {
                None => el.remove_and_keep_content(), // javascript: and friends
                Some(abs) => {
                    let is_ad = !abs.starts_with('#')
                        && filters.is_some_and(|f| f.is_blocked(&abs, page, RequestKind::Other));
                    if is_ad {
                        el.remove();
                    } else {
                        let clean = Url::parse(&abs)
                            .map(|u| crate::bypass::strip_tracking(&u).to_string())
                            .unwrap_or(abs);
                        el.set_attribute("href", &clean)?;
                    }
                }
            }
            Ok(())
        }))
        .append_element_content_handler(element!("*", |el| {
            if el.removed() {
                return Ok(());
            }
            let is_osprey = el.get_attribute("class").is_some_and(|c| c.starts_with("osprey-"));
            let drop: Vec<String> = el
                .attributes()
                .iter()
                .map(|a| a.name())
                .filter(|n| !(KEEP_ATTRS.contains(&n.as_str()) || (is_osprey && n == "class")))
                .collect();
            for name in drop {
                el.remove_attribute(&name);
            }
            Ok(())
        }))
        .append_element_content_handler(element!("head", |el| {
            el.append(STYLE, ContentType::Html);
            Ok(())
        }))
        .append_element_content_handler(element!("body", |el| {
            if opts.banner {
                let url = escape(page);
                el.prepend(
                    &format!(r#"<div class="osprey-banner">Text-only view of <a href="{url}">{url}</a></div>"#),
                    ContentType::Html,
                );
            }
            Ok(())
        }));

    rewrite_str(html, settings).map_err(rewrite_error)
}

/// Plain text version with numbered link references at the end, wrapped
/// at `width` columns.
pub fn to_text(simplified_html: &str, width: usize) -> Result<String> {
    html2text::config::plain()
        .link_footnotes(true)
        .string_from_read(simplified_html.as_bytes(), width.max(20))
        .map_err(|e| Error::Parse(format!("could not render text: {e}")))
}

/// Loads a page over HTTP, or from a local file path.
pub async fn load_page(client: &reqwest::Client, location: &str) -> Result<(Url, String)> {
    let path = std::path::Path::new(location);
    if !location.contains("://") && path.exists() {
        let abs = std::fs::canonicalize(path)?;
        let url = Url::from_file_path(&abs)
            .map_err(|_| Error::Unsupported(format!("cannot turn {} into a URL", abs.display())))?;
        let text = String::from_utf8_lossy(&tokio::fs::read(&abs).await?).into_owned();
        return Ok((url, text));
    }
    let resp = crate::client::ensure_success(client.get(location).send().await?)?;
    let url = resp.url().clone();
    Ok((url, resp.text().await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<!doctype html><html><head>
<title>Test page</title><meta name="description" content="x"><meta charset="utf-8">
<link rel="stylesheet" href="/s.css"><style>body{background:url(bg.gif)}</style>
<script>alert(1)</script></head>
<body style="color:red" onload="evil()">
<h1 class="headline">Hello</h1>
<p>Read the <a href="/docs/intro" onclick="track()">intro</a> or <a href="javascript:void(0)">nothing</a>.</p>
<img src="/cat.gif" alt="A dancing cat">
<img src="/spacer.gif" width="1" height="1">
<picture><source srcset="/a.webp"><img src="/a.jpg" alt="Chart of sales"></picture>
<video src="/v.mp4"></video><iframe src="https://ads.test/frame"></iframe><svg><circle r="4"/></svg>
<div class="adsbygoogle">AD TEXT</div>
<div class="promo-box">PROMO</div>
<p>Sponsored: <a href="https://ad.doubleclick.net/click?x=1">buy now</a></p>
<form><input name="q"><button>Go</button></form>
</body></html>"#;

    fn run(filters: Option<&Filters>, opts: &TextModeOptions) -> String {
        simplify(
            PAGE,
            &Url::parse("https://site.test/blog/post").unwrap(),
            filters,
            opts,
        )
        .unwrap()
    }

    #[test]
    fn strips_media_scripts_and_styles() {
        let out = run(None, &TextModeOptions::default());
        for gone in [
            "<script",
            "alert(1)",
            "<img",
            "<video",
            "<iframe",
            "<svg",
            "<picture",
            "<form",
            "<input",
            "s.css",
            "onclick",
            "onload",
            "style=\"color",
            "spacer.gif",
            "srcset",
            "name=\"description\"",
        ] {
            assert!(!out.contains(gone), "'{gone}' should be gone:\n{out}");
        }
        assert!(out.contains("<h1>Hello</h1>"), "{out}");
        assert!(
            out.contains(r#"<a href="https://site.test/docs/intro">intro</a>"#),
            "{out}"
        );
        assert!(
            out.contains(
                r#"<a class="osprey-img" href="https://site.test/cat.gif">[image: A dancing cat]</a>"#
            ),
            "{out}"
        );
        assert!(out.contains("[image: Chart of sales]"));
        assert!(out.contains("nothing"));
        assert!(!out.contains("javascript:"));
        assert!(out.contains("Text-only view of"));
        assert!(out.contains(r#"<meta charset="utf-8">"#));
        // Without filters, ad blocks are just text
        assert!(out.contains("AD TEXT"));
    }

    #[test]
    fn filters_remove_ads_and_ad_links() {
        let f = Filters::from_rules("||doubleclick.net^\n##.adsbygoogle\nsite.test##.promo-box\n");
        let out = run(Some(&f), &TextModeOptions::default());
        assert!(!out.contains("AD TEXT"), "{out}");
        assert!(!out.contains("PROMO"), "{out}");
        assert!(!out.contains("buy now"), "{out}");
        assert!(out.contains("Sponsored:"));
        assert!(out.contains("intro"));
    }

    #[test]
    fn image_links_can_be_turned_off() {
        let opts = TextModeOptions {
            image_links: false,
            banner: false,
        };
        let out = run(None, &opts);
        assert!(!out.contains("[image:"));
        assert!(!out.contains("Text-only view"));
    }

    #[test]
    fn plain_text_has_link_references() {
        let html = run(
            None,
            &TextModeOptions {
                image_links: true,
                banner: false,
            },
        );
        let text = to_text(&html, 80).unwrap();
        assert!(text.contains("Hello"));
        assert!(text.contains("https://site.test/docs/intro"), "{text}");
        assert!(!text.contains("alert"));
    }

    #[test]
    fn works_on_fragments() {
        let out = simplify(
            "<p>Hi <img src=x.png alt=pic></p>",
            &Url::parse("https://f.test/").unwrap(),
            None,
            &TextModeOptions::default(),
        )
        .unwrap();
        assert!(out.contains("[image: pic]"));
        assert!(out.contains("<style>"));
    }
}
