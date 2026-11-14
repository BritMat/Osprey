//! The Windows hook for native capture: WebView2's
//! WebResourceResponseReceived event. It reports the request as it was
//! really sent, with the headers the network stack added, and the response.

use tauri::{AppHandle, Webview};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2HttpResponseHeaders, ICoreWebView2WebResourceRequest,
    ICoreWebView2WebResourceResponseReceivedEventArgs, ICoreWebView2_2,
};
use webview2_com::{take_pwstr, WebResourceResponseReceivedEventHandler};
use windows_core::{Interface, BOOL, HSTRING, PWSTR};

use crate::capture::{self, Seen};

/// Starts capture for a tab. The first request of a new tab can get ahead
/// of this, which only ever loses the page itself, and the sniffer still
/// sees a media file opened directly.
pub fn attach(app: &AppHandle, wv: &Webview, tab: u32) {
    let app = app.clone();
    let done = wv.with_webview(move |platform| {
        // SAFETY: the controller belongs to this tab and this closure runs
        // on the window thread, where WebView2 objects must be used.
        let result = unsafe {
            platform.controller().CoreWebView2().and_then(|core| {
                let core = core.cast::<ICoreWebView2_2>()?;
                let handler =
                    WebResourceResponseReceivedEventHandler::create(Box::new(move |_, args| {
                        if let Some(args) = args {
                            let _ = on_response(&app, tab, &args);
                        }
                        Ok(())
                    }));
                let mut token = 0i64;
                core.add_WebResourceResponseReceived(&handler, &mut token)
            })
        };
        if let Err(e) = result {
            eprintln!("request capture is off for tab {tab}: {e}");
        }
    });
    if let Err(e) = done {
        eprintln!("request capture is off for tab {tab}: {e}");
    }
}

/// SAFETY: call on the window thread, inside the event.
unsafe fn on_response(
    app: &AppHandle,
    tab: u32,
    args: &ICoreWebView2WebResourceResponseReceivedEventArgs,
) -> windows_core::Result<()> {
    let request = args.Request()?;
    let response = args.Response()?;
    let mut url = PWSTR::null();
    request.Uri(&mut url)?;
    let mut method = PWSTR::null();
    request.Method(&mut method)?;
    let mut status = 0i32;
    response.StatusCode(&mut status)?;
    let headers = response.Headers()?;
    let seen = Seen {
        url: take_pwstr(url),
        method: take_pwstr(method),
        status: u16::try_from(status).unwrap_or(0),
        content_type: response_header(&headers, "Content-Type"),
        content_range: response_header(&headers, "Content-Range"),
        content_length: response_header(&headers, "Content-Length").and_then(|v| v.parse().ok()),
    };
    capture::on_response(app, tab, seen, || {
        request_headers(&request).unwrap_or_default()
    });
    Ok(())
}

/// SAFETY: as for [`on_response`].
unsafe fn response_header(
    headers: &ICoreWebView2HttpResponseHeaders,
    name: &str,
) -> Option<String> {
    let name = HSTRING::from(name);
    let mut found = BOOL(0);
    headers.Contains(&name, &mut found).ok()?;
    if !found.as_bool() {
        return None;
    }
    let mut value = PWSTR::null();
    headers.GetHeader(&name, &mut value).ok()?;
    Some(take_pwstr(value))
}

/// SAFETY: as for [`on_response`].
unsafe fn request_headers(
    request: &ICoreWebView2WebResourceRequest,
) -> windows_core::Result<Vec<(String, String)>> {
    let list = request.Headers()?.GetIterator()?;
    let mut out = Vec::new();
    let mut more = BOOL(0);
    list.HasCurrentHeader(&mut more)?;
    while more.as_bool() {
        let (mut name, mut value) = (PWSTR::null(), PWSTR::null());
        list.GetCurrentHeader(&mut name, &mut value)?;
        out.push((take_pwstr(name), take_pwstr(value)));
        list.MoveNext(&mut more)?;
    }
    Ok(out)
}
