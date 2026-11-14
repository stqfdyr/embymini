//! 检查更新与自动安装（官方 updater 插件；签名用编进程序的公钥校验）。

use crate::App;
use crate::emby::Error;
use serde::Serialize;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_updater::UpdaterExt;

/// 更新代理：`http://`、`https://`、`socks5://`、`socks5h://` 加主机和端口。
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

/// 检查更新和下载安装包用的 updater：设了更新代理就走它，否则跟随系统代理
/// （updater 每次都新建 HTTP 客户端，现读系统代理设置，不会像 Emby 的客户端那样读到过期的）。
fn updater(handle: &AppHandle, app: &App) -> Result<tauri_plugin_updater::Updater, Error> {
    // 40 MB 的安装包在慢代理上也要能下完；连不上时 reqwest 照常很快报错
    let mut b = handle.updater_builder().timeout(Duration::from_secs(900));
    if let Some(p) = app.cfg().update_proxy.clone() {
        b = b.proxy(proxy_url(&p)?);
    }
    b.build()
        .map_err(|e| Error::Other(format!("检查更新失败：{e}")))
}

/// 有新版本时返回版本号和更新说明（Release 里的 latest.json，签名用编进程序的公钥校验）。
#[tauri::command]
pub(crate) async fn update_check(
    handle: AppHandle,
    app: State<'_, App>,
) -> Result<Option<UpdateInfo>, Error> {
    let u = updater(&handle, &app)?.check().await.map_err(|e| {
        log::warn!("检查更新失败：{e}");
        Error::Other(format!("检查更新失败：{e}"))
    })?;
    Ok(u.map(|u| UpdateInfo {
        version: u.version,
        notes: u.body.unwrap_or_default(),
    }))
}

/// 下载并安装新版本：进度经 `update-progress` 事件发给界面（已下载字节, 总字节）。
/// 下完先关播放器、回传进度，再启动安装器（只显示进度条，不用点），安装器装完自动重新打开程序。
#[tauri::command]
pub(crate) async fn update_install(handle: AppHandle, app: State<'_, App>) -> Result<(), Error> {
    let fail = |e: tauri_plugin_updater::Error| {
        log::warn!("更新失败：{e}");
        Error::Other(format!("更新失败：{e}"))
    };
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
                // 每个分块都发的话 40 MB 要发几千次：百分比（不知道总大小时按 MB）变了才发
                let step = total.map_or(got >> 20, |t| got * 100 / t.max(1));
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
    // 安装器起来后进程直接退出，不走 RunEvent::Exit：先关播放器、回传进度。
    // shutdown 内部用 block_on，在 async 命令里直接调会 panic，命令永远不返回、界面停在 100%
    let h = handle.clone();
    let _ = tauri::async_runtime::spawn_blocking(move || {
        h.state::<App>().shutdown(Duration::from_secs(3));
    })
    .await;
    // 进程马上被安装器结束，不走 RunEvent::Exit：先撤掉运行标记，否则下次启动会记成异常退出
    let root = app.path.parent().unwrap_or(std::path::Path::new("."));
    crate::set_running(root, false);
    // Windows 上启动安装器后直接退出进程，不会返回
    if let Err(e) = u.install(bytes) {
        crate::set_running(root, true);
        // 安装器没起来，程序照常用：撤销退出标记，否则之后点播放都会被当成「正在退出」而放弃
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
            "socks5://127.0.0.1:10808",
            "socks5h://localhost:1080",
        ] {
            assert!(proxy_url(ok).is_ok(), "{ok}");
        }
        for bad in ["127.0.0.1:10808", "ftp://h:1", "socks5://", ""] {
            assert!(proxy_url(bad).is_err(), "{bad}");
        }
    }
}
