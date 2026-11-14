// The command list makes Tauri generate one permission per command, so
// capabilities can grant them one by one. Web pages in tabs get only
// `media_found`, `tab_shortcut` and `gm_call`, the one a user script uses and
// which checks every call itself. The rest is for the app's own UI.
const COMMANDS: &[&str] = &[
    "ui_ready",
    "tab_new",
    "tab_close",
    "tab_activate",
    "tab_navigate",
    "tab_back",
    "tab_forward",
    "tab_reload",
    "tab_stop",
    "tab_text_mode",
    "layout",
    "net_set",
    "net_status",
    "ads_set",
    "media_download",
    "download_cancel",
    "download_open_folder",
    "clear_browsing_data",
    "media_found",
    "tab_shortcut",
    "prefs_set",
    "download_pause",
    "download_resume",
    "download_open",
    "download_show",
    "media_preview",
    "media_thumb",
    "settings_view",
    "settings_set",
    "settings_export",
    "settings_import",
    "filters_update",
    "filters_rules_set",
    "tools_view",
    "tools_update",
    "cache_clear",
    "probe_preview",
    "gm_call",
    "scripts_view",
    "script_install",
    "script_toggle",
    "script_remove",
    "script_update",
    "script_menu_run",
    "script_answer",
    "link_resolve",
    "link_open_vip",
    "bypass_key_set",
    "extension_add",
    "extension_remove",
];

fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)),
    )
    .expect("tauri build step failed");
}
