//! Local config: server list, active server, playback settings. One JSON file, `config.json` under [`data_root`].

use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

/// Marker file in the data dir. The uninstaller deletes `data\` only when it is present, so installing into a
/// folder that already had someone else's `data` never wipes it. Renaming it needs `windows/hooks.nsh` too.
pub const MARKER: &str = ".embymini";

/// Data root: everything (config, logs, UI cache, mpv cache) lives in `data\` beside the exe, nothing on the
/// system drive. The installer is per-user and unelevated, so the chosen folder is writable; if not (e.g.
/// copied to a read-only place) falls back to `%LOCALAPPDATA%\<identifier>`.
pub fn data_root(identifier: &str) -> PathBuf {
    let beside_exe = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| dunce::simplified(d).join("data")));
    beside_exe.filter(|d| claim(d)).unwrap_or_else(|| {
        std::env::var_os("LOCALAPPDATA")
            .map_or_else(std::env::temp_dir, PathBuf::from)
            .join(identifier)
    })
}

/// Whether `d` is usable (writable) as the data dir. Only a new, empty or already-ours dir gets the marker;
/// a `data` holding someone else's files (or one we cannot list) is used but never marked, so never uninstalled.
fn claim(d: &Path) -> bool {
    let ours = d.join(MARKER).exists()
        || fs::read_dir(d).map_or_else(
            |e| e.kind() == io::ErrorKind::NotFound,
            |mut r| r.next().is_none(),
        );
    if fs::create_dir_all(d).is_err() {
        return false;
    }
    if ours {
        return fs::write(d.join(MARKER), b"").is_ok();
    }
    let probe = d.join(".embymini-probe");
    let ok = fs::write(&probe, b"").is_ok();
    let _ = fs::remove_file(probe);
    ok
}

/// Old versions kept config (config.json, mpv.conf) in `%APPDATA%\<id>` and logs / caches in `%LOCALAPPDATA%\<id>`.
/// If the new root has no config.json yet, copy both files over (may cross drives), then delete `old_dirs`
/// except one containing the root. config.json is copied last: its presence means the move finished.
/// Returns whether anything was moved.
pub fn migrate(root: &Path, old_config: &Path, old_dirs: &[PathBuf]) -> io::Result<bool> {
    if root.join("config.json").exists() || !old_config.join("config.json").is_file() {
        return Ok(false);
    }
    fs::create_dir_all(root)?;
    for f in ["mpv.conf", "config.json"] {
        let src = old_config.join(f);
        if src.is_file() {
            fs::copy(&src, root.join(f))?;
        }
    }
    for d in old_dirs.iter().filter(|d| !root.starts_with(d)) {
        let _ = fs::remove_dir_all(d);
    }
    Ok(true)
}

// No `Debug`: it would print server tokens if anyone ever logged a config.
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    /// This machine's device id, shared by all servers; generated on first start.
    pub device_id: String,
    pub active: Option<String>,
    pub servers: Vec<Server>,
    /// mpv executable; empty means the bundled one, then PATH.
    pub mpv_path: Option<String>,
    /// Preload the next episode once the current one reaches this percentage.
    pub preload_percent: u8,
    /// Volume when the player last quit; reused on the next start.
    pub volume: Option<f64>,
    /// Proxy for update checks and installer downloads only (e.g. `http://127.0.0.1:10808`); empty follows
    /// the system proxy. Every other request bypasses proxies.
    pub update_proxy: Option<String>,
    /// Cache the whole video while playing (default on); off caches only a window ahead of playback.
    pub cache_whole: bool,
    /// Danmaku: API address, blocked words, display settings, manual matches.
    pub danmaku: crate::danmaku::Config,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device_id: uuid::Uuid::new_v4().to_string(),
            active: None,
            servers: Vec::new(),
            mpv_path: None,
            preload_percent: 80,
            volume: None,
            update_proxy: None,
            cache_whole: true,
            danmaku: crate::danmaku::Config::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Server {
    pub id: String,
    pub name: String,
    /// `scheme://host[/subpath]` without the port, see [`normalize`].
    pub url: String,
    pub port: u16,
    pub user: String,
    #[serde(default)]
    pub user_id: String,
    /// Empty means a fresh login is needed. Only the token is stored, never the password.
    #[serde(default)]
    pub token: String,
}

/// Server info for the UI; never carries the token.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerView {
    pub id: String,
    pub name: String,
    pub url: String,
    pub port: u16,
    pub user: String,
    pub user_id: String,
    /// API root; the UI builds image URLs from it (images need no token).
    pub base: String,
    pub logged_in: bool,
}

impl Server {
    /// API root: `scheme://host:port[/subpath]/emby`.
    pub fn api_base(&self) -> String {
        match Url::parse(&self.url) {
            Ok(mut u) => {
                let _ = u.set_port(Some(self.port));
                format!("{}/emby", u.as_str().trim_end_matches('/'))
            }
            Err(_) => format!("{}/emby", self.url),
        }
    }

    pub fn view(&self) -> ServerView {
        ServerView {
            id: self.id.clone(),
            name: self.name.clone(),
            url: self.url.clone(),
            port: self.port,
            user: self.user.clone(),
            user_id: self.user_id.clone(),
            base: self.api_base(),
            logged_in: !self.token.is_empty(),
        }
    }
}

/// Upstream tag: FNV-1a of the API root, top 8 hex digits. Video cache dirs, danmaku cache and manual matches
/// are keyed by it so file names never contain addresses. Never change the algorithm: existing caches and
/// matches would stop matching.
pub fn server_tag(api_base: &str) -> String {
    let h = api_base.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{:08x}", h >> 32)
}

/// Normalizes the user's address and port into `(scheme://host[/subpath], port)`.
///
/// Accepts bare hosts, schemes, ports inside the address, browser URLs like `…/web/index.html#!/home`,
/// and a trailing `/emby`. Port precedence: port field > port in the address > scheme default.
/// Without a scheme it is https, except http when the port is 80 or 8096 (Emby's http default).
pub fn normalize(input: &str, port: Option<u16>) -> Result<(String, u16), String> {
    let s = input.trim();
    let has_scheme = s.contains("://");
    let full = if has_scheme {
        s.to_owned()
    } else {
        format!("https://{s}")
    };
    let mut u = Url::parse(&full).map_err(|_| format!("地址格式不对：{s}"))?;
    if !has_scheme && matches!(port.or(u.port()), Some(80 | 8096)) {
        // Only the scheme changes; Url keeps a port written in the address.
        u = Url::parse(&format!("http://{s}")).map_err(|_| format!("地址格式不对：{s}"))?;
    }
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none_or(str::is_empty) {
        return Err(format!("只支持 http / https 地址：{s}"));
    }
    let port = port
        .or(u.port())
        .or(u.port_or_known_default())
        .ok_or_else(|| format!("无法确定端口：{s}"))?;
    if port == 0 {
        return Err("端口不能是 0".into());
    }
    // Keep the subpath only up to /web or /emby (a reverse proxy may mount Emby under a subpath).
    let path: Vec<&str> = u
        .path_segments()
        .map(|it| {
            it.take_while(|p| !p.eq_ignore_ascii_case("web") && !p.eq_ignore_ascii_case("emby"))
                .filter(|p| !p.is_empty())
                .collect()
        })
        .unwrap_or_default();
    u.set_path(&path.join("/"));
    u.set_query(None);
    u.set_fragment(None);
    let _ = u.set_port(None);
    Ok((u.as_str().trim_end_matches('/').to_owned(), port))
}

/// Startup saves right after loading, so this must never hand back defaults while the user's file is still
/// in place. Missing file: defaults. Corrupt file: moved to a new `config.json.bad[.N]` (older backups kept),
/// then defaults. Unreadable file (e.g. briefly locked by antivirus): retried, then an error that stops startup.
pub fn load(path: &Path) -> io::Result<Config> {
    let mut retries = 3;
    let bytes = loop {
        match fs::read(path) {
            Ok(b) => break b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) if retries > 0 => {
                log::warn!("读取配置失败，稍后重试：{e}");
                retries -= 1;
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    };
    serde_json::from_slice(&bytes).or_else(|e| {
        let bad = (1..)
            .map(|n| match n {
                1 => path.with_extension("json.bad"),
                n => path.with_extension(format!("json.bad.{n}")),
            })
            .find(|p| !p.exists())
            .expect("unbounded");
        log::warn!("配置文件损坏，改名为 {} 后重建：{e}", bad.display());
        fs::rename(path, bad)?;
        Ok(Config::default())
    })
}

/// Writes a temp file, syncs it and renames it over, so a crash or power cut never leaves a broken config.
pub fn save(path: &Path, cfg: &Config) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let bytes = serde_json::to_vec_pretty(cfg)?;
    let tmp = path.with_extension("json.tmp");
    // The file is closed before the rename (Windows refuses to rename an open file).
    let written = fs::File::create(&tmp).and_then(|mut f| {
        f.write_all(&bytes)?;
        f.sync_all()
    });
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    fs::rename(tmp, path)
}

/// Applies `f` to a copy and saves it; `cfg` changes only once the save succeeded, so memory never
/// runs ahead of disk (a failed add would otherwise still list the server, and a retry duplicate it).
pub fn update<T>(path: &Path, cfg: &mut Config, f: impl FnOnce(&mut Config) -> T) -> io::Result<T> {
    let mut next = cfg.clone();
    let out = f(&mut next);
    save(path, &next)?;
    *cfg = next;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_inputs() {
        let ok = |i: &str, p: Option<u16>| normalize(i, p).unwrap();
        assert_eq!(
            ok("emby.example.com", None),
            ("https://emby.example.com".into(), 443)
        );
        assert_eq!(
            ok("https://emby.example.com", Some(443)),
            ("https://emby.example.com".into(), 443)
        );
        assert_eq!(ok("1.2.3.4", Some(8096)), ("http://1.2.3.4".into(), 8096));
        // Common on a LAN: port in the address, no scheme.
        assert_eq!(
            ok("192.168.1.5:8096", None),
            ("http://192.168.1.5".into(), 8096)
        );
        assert_eq!(
            ok("nas.local:8920", None),
            ("https://nas.local".into(), 8920)
        );
        assert_eq!(ok("nas.local:80", None), ("http://nas.local".into(), 80));
        assert_eq!(
            ok("http://1.2.3.4:8096/web/index.html#!/home", None),
            ("http://1.2.3.4".into(), 8096)
        );
        assert_eq!(
            ok("https://h.example/sub/emby/", Some(8920)),
            ("https://h.example/sub".into(), 8920)
        );
        assert_eq!(
            ok("  https://h.example:8443  ", None),
            ("https://h.example".into(), 8443)
        );
        assert!(normalize("ftp://h.example", None).is_err());
        assert!(normalize("", None).is_err());
        assert_eq!(normalize("h.example", Some(0)), Err("端口不能是 0".into()));
        assert_eq!(normalize("h.example:0", None), Err("端口不能是 0".into()));
    }

    #[test]
    fn server_tag_is_stable() {
        // Fixed values: changing the algorithm orphans existing video caches and danmaku matches.
        assert_eq!(server_tag(""), "cbf29ce4");
        assert_eq!(server_tag("https://h.example/emby"), "74dce48f");
        assert_ne!(
            server_tag("https://a.example/emby"),
            server_tag("https://b.example/emby")
        );
    }

    #[test]
    fn api_base_carries_port_and_subpath() {
        let s = |url: &str, port| Server {
            id: "1".into(),
            name: String::new(),
            url: url.into(),
            port,
            user: String::new(),
            user_id: String::new(),
            token: String::new(),
        };
        assert_eq!(
            s("https://h.example", 443).api_base(),
            "https://h.example/emby"
        );
        assert_eq!(
            s("https://h.example", 8920).api_base(),
            "https://h.example:8920/emby"
        );
        assert_eq!(
            s("http://1.2.3.4/sub", 8096).api_base(),
            "http://1.2.3.4:8096/sub/emby"
        );
        assert!(!s("https://h.example", 443).view().logged_in);
        let v = Server {
            token: "t".into(),
            ..s("https://h.example", 443)
        }
        .view();
        assert!(v.logged_in);
        assert_eq!(v.base, "https://h.example/emby");
    }

    #[test]
    fn load_save_roundtrip_and_corrupt_file() {
        let dir = std::env::temp_dir().join(format!("embymini-cfg-{}", std::process::id()));
        let path = dir.join("config.json");
        let c = Config {
            preload_percent: 90,
            ..Default::default()
        };
        save(&path, &c).unwrap();
        assert_eq!(load(&path).unwrap().device_id, c.device_id);
        assert!(!path.with_extension("json.tmp").exists());
        // A second corruption keeps the first backup.
        for (i, junk) in [&b"{not json"[..], b"nope"].into_iter().enumerate() {
            fs::write(&path, junk).unwrap();
            assert_eq!(load(&path).unwrap().preload_percent, 80);
            let bad = ["json.bad", "json.bad.2"][i];
            assert_eq!(fs::read(path.with_extension(bad)).unwrap(), junk);
        }
        assert!(!path.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unreadable_config_stops_startup() {
        let dir = std::env::temp_dir().join(format!("embymini-unread-{}", std::process::id()));
        let path = dir.join("config.json");
        // A directory in its place: reading fails with something other than NotFound.
        fs::create_dir_all(&path).unwrap();
        assert!(load(&path).is_err());
        assert!(path.is_dir(), "left in place");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn update_keeps_memory_when_save_fails() {
        let dir = std::env::temp_dir().join(format!("embymini-upd-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("file"), b"").unwrap();
        let mut c = Config::default();
        // Parent is a regular file, so the save fails.
        let bad = dir.join("file").join("config.json");
        assert!(update(&bad, &mut c, |c| c.preload_percent = 60).is_err());
        assert_eq!(c.preload_percent, 80);
        let good = dir.join("config.json");
        let out = update(&good, &mut c, |c| {
            c.preload_percent = 60;
            7
        });
        assert_eq!(out.unwrap(), 7);
        assert_eq!(c.preload_percent, 60);
        assert_eq!(load(&good).unwrap().preload_percent, 60);
        fs::remove_dir_all(dir).unwrap();
    }

    /// Only new, empty or already-ours dirs get the uninstall marker; someone else's `data` is used unmarked.
    #[test]
    fn claim_marks_only_our_dirs() {
        let t = std::env::temp_dir().join(format!("embymini-claim-{}", std::process::id()));
        let (fresh, foreign) = (t.join("fresh"), t.join("foreign"));
        assert!(claim(&fresh) && fresh.join(MARKER).exists());
        assert!(claim(&fresh), "second start still usable");
        fs::create_dir_all(&foreign).unwrap();
        fs::write(foreign.join("别人的文件"), b"x").unwrap();
        assert!(claim(&foreign));
        assert!(!foreign.join(MARKER).exists());
        assert_eq!(
            fs::read_dir(&foreign).unwrap().count(),
            1,
            "probe file removed"
        );
        fs::remove_dir_all(t).unwrap();
    }

    #[test]
    fn migrate_moves_config_once_and_cleans_old_dirs() {
        let t = std::env::temp_dir().join(format!("embymini-mig-{}", std::process::id()));
        let (root, old_cfg, old_local) = (t.join("data"), t.join("roaming"), t.join("local"));
        fs::create_dir_all(&old_cfg).unwrap();
        fs::create_dir_all(old_local.join("logs")).unwrap();
        fs::write(old_cfg.join("config.json"), b"{}").unwrap();
        fs::write(old_cfg.join("mpv.conf"), b"volume=50").unwrap();
        let old = [old_cfg.clone(), old_local.clone()];
        assert!(migrate(&root, &old_cfg, &old).unwrap());
        assert_eq!(fs::read(root.join("mpv.conf")).unwrap(), b"volume=50");
        assert!(root.join("config.json").is_file());
        assert!(!old_cfg.exists() && !old_local.exists());
        // New location already has a config: do nothing.
        fs::create_dir_all(&old_cfg).unwrap();
        fs::write(old_cfg.join("config.json"), b"{}").unwrap();
        assert!(!migrate(&root, &old_cfg, &old).unwrap());
        assert!(old_cfg.exists());
        // When the old LOCALAPPDATA dir is the fallback root, it must not delete itself.
        let root2 = old_local.join("data");
        fs::create_dir_all(&old_local).unwrap();
        assert!(migrate(&root2, &old_cfg, &[old_cfg.clone(), old_local.clone()]).unwrap());
        assert!(root2.join("config.json").is_file());
        fs::remove_dir_all(t).unwrap();
    }
}
