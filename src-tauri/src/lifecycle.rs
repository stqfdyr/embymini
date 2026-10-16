//! 进程生命周期的诊断：关闭各阶段耗时（`exit`）、异常退出检测（`crash`）。

use crate::{emby, telemetry};
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

// ---------- 关闭耗时埋点 ----------
// 从点关闭（或开始退出）算起，各阶段的毫秒数写日志；退出前存到 data\last-exit.json，
// 下次启动作为诊断数据 `exit` 上报，用来查「关闭时卡一下」卡在哪一段。

static CLOSE_T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
/// 进程启动的时刻（单调时钟 + 墙上时间毫秒）：使用时长、启动耗时都从这里算。
pub(crate) static START: std::sync::OnceLock<(std::time::Instant, u64)> =
    std::sync::OnceLock::new();

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn uptime_s() -> u64 {
    START.get().map_or(0, |(t, _)| t.elapsed().as_secs())
}
static CLOSE_MARKS: Mutex<Vec<(&'static str, u64)>> = Mutex::new(Vec::new());

/// 开始计时（重复调用以第一次为准）。
pub(crate) fn exit_start() {
    CLOSE_T0.get_or_init(std::time::Instant::now);
}

/// 记一个关闭阶段；还没开始关闭时（例如平时关掉 mpv）什么都不做。
pub(crate) fn exit_mark(phase: &'static str) {
    let Some(t0) = CLOSE_T0.get() else { return };
    let ms = t0.elapsed().as_millis() as u64;
    log::info!("关闭耗时 {phase}：{ms} ms");
    crate::lock(&CLOSE_MARKS).push((phase, ms));
}

fn exit_file(root: &std::path::Path) -> PathBuf {
    root.join("last-exit.json")
}

/// 退出前把各阶段耗时存盘（进程马上结束，诊断数据的写入任务来不及落盘）。
pub(crate) fn exit_save(root: &std::path::Path) {
    let mut marks: Map<String, Value> = crate::lock(&CLOSE_MARKS)
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).into()))
        .collect();
    if !marks.is_empty() {
        // 下次启动才报，那时可能已经升级：带上这次的版本；运行时长是使用时长的分母
        marks.insert("version".into(), emby::VERSION.into());
        marks.insert("uptime_s".into(), uptime_s().into());
        let _ = std::fs::write(exit_file(root), Value::Object(marks).to_string());
    }
}

// ---------- 异常退出检测 ----------
// 启动时写 data\running.json（版本、启动时刻、最后存活时刻），每分钟刷新存活时刻，正常退出（含自动更新）删掉。
// 下次启动时它还在 = 上次没走正常退出：崩溃、被结束进程、关机时被杀，记一条 `crash`。

fn running_file(root: &std::path::Path) -> PathBuf {
    root.join("running.json")
}

pub(crate) fn running_write(root: &std::path::Path) {
    let started = START.get().map_or(0, |(_, ms)| *ms);
    let v = serde_json::json!({ "version": emby::VERSION, "started": started, "alive": now_ms() });
    let _ = std::fs::write(running_file(root), v.to_string());
}

/// 启动时：上次的标记还在就补记 `crash`，然后写这次的标记，并每分钟刷新一次。
pub(crate) fn running_check(root: &std::path::Path) {
    let f = running_file(root);
    if let Some(v) = std::fs::read(&f)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    {
        let (started, alive) = (
            v["started"].as_u64().unwrap_or(0),
            v["alive"].as_u64().unwrap_or(0),
        );
        log::warn!(
            "上次没有正常退出（版本 {}）",
            v["version"].as_str().unwrap_or("?")
        );
        telemetry::rec(
            "crash",
            serde_json::json!({
                "version": v["version"], "uptime_s": alive.saturating_sub(started) / 1000,
                "ago_s": now_ms().saturating_sub(alive) / 1000,
            }),
        );
    }
    running_write(root);
    let root = root.to_owned();
    tauri::async_runtime::spawn(async move {
        let mut t = tokio::time::interval(Duration::from_secs(60));
        t.tick().await;
        loop {
            t.tick().await;
            running_write(&root);
        }
    });
}

pub(crate) fn running_clear(root: &std::path::Path) {
    let _ = std::fs::remove_file(running_file(root));
}

/// 启动时把上次退出的耗时补记成诊断数据（没开诊断就只删文件）。
pub(crate) fn exit_report(root: &std::path::Path) {
    let f = exit_file(root);
    if let Some(v) = std::fs::read(&f)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    {
        telemetry::rec("exit", v);
    }
    let _ = std::fs::remove_file(f);
}
