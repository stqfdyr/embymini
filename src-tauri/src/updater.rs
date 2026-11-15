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

/// Logs and words an updater error; network failures read like the Emby ones ([`crate::emby::net`]).
fn fail(what: &str, e: &tauri_plugin_updater::Error) -> Error {
    log::warn!("{what}：{e}");
    let reason = match e {
        tauri_plugin_updater::Error::Reqwest(r) => crate::emby::net(r).to_string(),
        e => e.to_string(),
    };
    Error::Other(format!("{what}：{reason}"))
}

/// Percent downloaded (MB when the size is unknown): progress is emitted only when it changes, not per chunk.
fn progress_step(got: u64, total: Option<u64>) -> u64 {
    total.map_or(got >> 20, |t| got * 100 / t.max(1))
}

/// Updater for the check and the download: through the update proxy if set, else the system proxy (the plugin
/// builds a fresh HTTP client each time, so it reads the current system setting, unlike the Emby client).
fn updater(handle: &AppHandle, app: &App) -> Result<tauri_plugin_updater::Updater, Error> {
    // A 40 MB package must finish over a slow proxy; an unreachable host still fails fast.
    let mut b = handle.updater_builder().timeout(Duration::from_secs(900));
    if let Some(p) = app.cfg().update_proxy.clone() {
        b = b.proxy(proxy_url(&p)?);
    }
    b.build().map_err(|e| fail("检查更新失败", &e))
}

/// Version and notes when a newer release exists (`latest.json` of the latest Release).
#[tauri::command]
pub(crate) async fn update_check(
    handle: AppHandle,
    app: State<'_, App>,
) -> Result<Option<UpdateInfo>, Error> {
    let u = updater(&handle, &app)?
        .check()
        .await
        .map_err(|e| fail("检查更新失败", &e))?;
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
    let fail = |e: tauri_plugin_updater::Error| fail("更新失败", &e);
    let u = updater(&handle, &app)?
        .check()
        .await
        .map_err(fail)?
        .ok_or_else(|| Error::Other("已是最新版本".into()))?;
    log::info!("下载新版本 {}", u.version);
    let (mut got, mut shown) = (0u64, u64::MAX);
    let bytes = u
        .download(
            |n, total| {
                got += n as u64;
                let step = progress_step(got, total);
                if step != shown {
                    shown = step;
                    let _ = handle.emit("update-progress", (got, total));
                }
            },
            || {},
        )
        .await
        .map_err(fail)?;
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
        return Err(fail(e));
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
    fn progress_steps_by_percent_or_mb() {
        assert_eq!(progress_step(0, Some(200)), 0);
        assert_eq!(progress_step(3, Some(200)), 1);
        assert_eq!(progress_step(200, Some(200)), 100);
        // A zero total must not divide by zero.
        assert_eq!(progress_step(5, Some(0)), 500);
        assert_eq!(progress_step((3 << 20) + 5, None), 3);
    }
}
