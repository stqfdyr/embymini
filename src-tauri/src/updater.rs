//! Update check and install (the official updater plugin; packages are verified with the built-in public key).

use crate::App;
use crate::emby::Error;
use serde::Serialize;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_updater::UpdaterExt;

/// Update proxy: `http://`, `https://`, `socks5://` or `socks5h://` plus host and port (or the scheme default).
/// Also the validation when settings are saved.
pub(crate) fn proxy_url(p: &str) -> Result<reqwest::Url, Error> {
    let bad = || {
        Error::Other(format!(
            "更新代理地址不对：{p}（例：http://127.0.0.1:10808 或 socks5://127.0.0.1:10808）"
        ))
    };
    let u = reqwest::Url::parse(p).map_err(|_| bad())?;
    let ok = matches!(u.scheme(), "http" | "https" | "socks5" | "socks5h")
        && u.host_str().is_some_and(|h| !h.is_empty())
        && u.port_or_known_default().is_some();
    ok.then_some(u).ok_or_else(bad)
}

#[derive(Serialize)]
pub(crate) struct UpdateInfo {
    version: String,
    notes: String,
}

const CHECK_FAILED: &str = "检查更新失败";
const INSTALL_FAILED: &str = "更新失败";

/// Why an updater call failed, in words: connection failures read like the Emby ones ([`crate::emby::net`]);
/// a bad reply (decode) or a bad proxy (builder) keeps reqwest's own text.
fn reason(e: &tauri_plugin_updater::Error) -> String {
    use tauri_plugin_updater::Error as U;
    match e {
        U::Reqwest(r) if !r.is_decode() && !r.is_builder() => crate::emby::net(r).to_string(),
        // The check got a non-2xx reply (GitHub rate limit, 404).
        U::ReleaseNotFound => "取不到版本信息（服务器拒绝或限流），稍后再试".into(),
        // The download got a non-2xx reply.
        U::Network(s) => format!("下载安装包被拒绝（{s}）"),
        e => e.to_string(),
    }
}

/// Maps an updater error to `<what>：<reason>` and logs it.
fn fail(what: &'static str) -> impl Fn(tauri_plugin_updater::Error) -> Error {
    move |e| {
        log::warn!("{what}：{e}");
        Error::Other(format!("{what}：{}", reason(&e)))
    }
}

/// Download progress: emitted only when the percent (MB when the size is unknown) changes, not per chunk.
#[derive(Default)]
struct Progress {
    got: u64,
    shown: Option<u64>,
}

impl Progress {
    /// Adds a chunk of `n` bytes; returns the bytes so far when the step changed.
    fn advance(&mut self, n: usize, total: Option<u64>) -> Option<u64> {
        self.got += n as u64;
        let step = total.map_or(self.got >> 20, |t| self.got * 100 / t.max(1));
        (self.shown.replace(step) != Some(step)).then_some(self.got)
    }
}

/// Updater for the check and the download: through the update proxy if set, else the system proxy (the plugin
/// builds a fresh HTTP client each time, so it reads the current system setting, unlike the Emby client).
fn updater(
    handle: &AppHandle,
    app: &App,
    what: &'static str,
) -> Result<tauri_plugin_updater::Updater, Error> {
    // A 40 MB package must finish over a slow proxy; an unreachable host still fails fast.
    let mut b = handle.updater_builder().timeout(Duration::from_secs(900));
    if let Some(p) = app.cfg().update_proxy.clone() {
        b = b.proxy(proxy_url(&p)?);
    }
    b.build().map_err(fail(what))
}

/// Version and notes when a newer release exists (`latest.json` of the latest Release).
#[tauri::command]
pub(crate) async fn update_check(
    handle: AppHandle,
    app: State<'_, App>,
) -> Result<Option<UpdateInfo>, Error> {
    let u = updater(&handle, &app, CHECK_FAILED)?
        .check()
        .await
        .map_err(fail(CHECK_FAILED))?;
    Ok(u.map(|u| UpdateInfo {
        version: u.version,
        notes: u.body.unwrap_or_default(),
    }))
}

/// Downloads and installs the new version, reporting `update-progress` as (bytes so far, total or null).
/// After the download the player is closed (progress reported back), then the installer runs unattended
/// and reopens the program when done.
#[tauri::command]
pub(crate) async fn update_install(handle: AppHandle, app: State<'_, App>) -> Result<(), Error> {
    let u = updater(&handle, &app, INSTALL_FAILED)?
        .check()
        .await
        .map_err(fail(INSTALL_FAILED))?
        .ok_or_else(|| Error::Other("已是最新版本".into()))?;
    log::info!("下载新版本 {}", u.version);
    let mut progress = Progress::default();
    let bytes = u
        .download(
            |n, total| {
                if let Some(got) = progress.advance(n, total) {
                    let _ = handle.emit("update-progress", (got, total));
                }
            },
            || {},
        )
        .await
        .map_err(fail(INSTALL_FAILED))?;
    log::info!("启动安装器，程序退出");
    // The installer ends the process without RunEvent::Exit, so close the player first. `shutdown` uses
    // block_on, which panics inside an async command: the command would never return (stuck at 100%).
    let h = handle.clone();
    let _ = tauri::async_runtime::spawn_blocking(move || {
        h.state::<App>().shutdown(Duration::from_secs(3));
    })
    .await;
    // Same reason: drop the running marker now, or the next start reports an abnormal exit.
    let root = app.path.parent().unwrap_or(std::path::Path::new("."));
    crate::set_running(root, false);
    // On Windows this starts the installer and exits the process: it only returns on failure.
    if let Err(e) = u.install(bytes) {
        crate::set_running(root, true);
        // The program stays in use: clear the quit flag, or every later play is dropped as "quitting".
        app.quit.send_replace(false);
        return Err(fail(INSTALL_FAILED)(e));
    }
    handle.restart()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_url_accepts_common_forms() {
        for ok in [
            "http://127.0.0.1:10808",
            "https://proxy.lan",
            "http://proxy.lan",
            "socks5://127.0.0.1:10808",
            "socks5h://localhost:1080",
        ] {
            assert!(proxy_url(ok).is_ok(), "{ok}");
        }
        // socks has no default port, so it must be written out.
        for bad in [
            "127.0.0.1:10808",
            "ftp://h:1",
            "socks5://",
            "socks5://h",
            "",
        ] {
            assert!(proxy_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn progress_emits_when_the_step_changes() {
        let mut p = Progress::default();
        let total = Some(200);
        assert_eq!(p.advance(1, total), Some(1)); // first chunk: 0 %
        assert_eq!(p.advance(1, total), Some(2)); // 1 %
        assert_eq!(p.advance(1, total), None); // still 1 %
        assert_eq!(p.advance(197, total), Some(200)); // 100 %
        // A zero total must not divide by zero.
        assert_eq!(Progress::default().advance(5, Some(0)), Some(5));
        // Unknown size: once per MB.
        let mut p = Progress::default();
        assert_eq!(p.advance(1 << 20, None), Some(1 << 20));
        assert_eq!(p.advance(5, None), None);
        assert_eq!(p.advance(1 << 20, None), Some((2 << 20) + 5));
    }

    #[test]
    fn http_refusals_are_worded() {
        use tauri_plugin_updater::Error as U;
        assert!(reason(&U::ReleaseNotFound).starts_with("取不到版本信息"));
        assert_eq!(
            reason(&U::Network("status: 404".into())),
            "下载安装包被拒绝（status: 404）"
        );
        assert_eq!(reason(&U::EmptyEndpoints), U::EmptyEndpoints.to_string());
    }
}
