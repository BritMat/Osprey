//! The Linux hook for native capture: WebKitGTK's resource-load-started
//! signal, then the resource's response as soon as it arrives. Waiting for
//! `finished` would be too late for a long video.

use tauri::{AppHandle, Webview};
use webkit2gtk::{
    URIRequest, URIRequestExt, URIResponseExt, WebResource, WebResourceExt, WebViewExt,
};

use crate::capture::{self, Seen};

/// Starts capture for a tab. The first request of a new tab can get ahead
/// of this, which only ever loses the page itself, and the sniffer still
/// sees a media file opened directly.
pub fn attach(app: &AppHandle, wv: &Webview, tab: u32) {
    let app = app.clone();
    let done = wv.with_webview(move |platform| {
        platform
            .inner()
            .connect_resource_load_started(move |_, resource, request| {
                let (app, request) = (app.clone(), request.clone());
                resource.connect_response_notify(move |resource| {
                    on_response(&app, tab, resource, &request);
                });
            });
    });
    if let Err(e) = done {
        eprintln!("request capture is off for tab {tab}: {e}");
    }
}

fn on_response(app: &AppHandle, tab: u32, resource: &WebResource, request: &URIRequest) {
    let Some(response) = resource.response() else {
        return;
    };
    let headers = response.http_headers();
    let seen = Seen {
        url: resource.uri().map(|u| u.to_string()).unwrap_or_default(),
        method: request
            .http_method()
            .map(|m| m.to_string())
            .unwrap_or_else(|| "GET".into()),
        status: u16::try_from(response.status_code()).unwrap_or(0),
        content_type: response.mime_type().map(|t| t.to_string()),
        content_range: headers
            .as_ref()
            .and_then(|h| h.one("Content-Range"))
            .map(|v| v.to_string()),
        content_length: Some(response.content_length()).filter(|n| *n > 0),
    };
    capture::on_response(app, tab, seen, || {
        let mut out = Vec::new();
        if let Some(headers) = request.http_headers() {
            headers.foreach(|k, v| out.push((k.to_string(), v.to_string())));
        }
        out
    });
}
