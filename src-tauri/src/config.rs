//! 本地配置：服务器列表、当前服务器、播放设置。存成一个 JSON 文件（数据目录下的 config.json，见 [`data_root`]）。

use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// 数据目录里的标记文件。卸载时只有它在，才删 `data\`：防止用户把程序直接装在一个
/// 本来就有 `data` 文件夹的目录里，卸载时误删别人的东西。改名要同步改 `windows/hooks.nsh`。
pub const MARKER: &str = ".embymini";

/// 数据根目录：所有数据（配置、日志、界面缓存、mpv 缓存）都放程序旁边的 `data\`，系统盘不留东西。
/// 安装器默认按当前用户安装、不提权，用户选的安装目录一定可写；
/// 不可写时（例如手动拷到只读位置）退回 `%LOCALAPPDATA%\<identifier>`。
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

/// 能否把 `d` 当数据目录（可写），顺带决定要不要打标记：只有新建的、空的或已经是我们的目录才打，
/// 本来就有别人东西的 `data` 照用但不打标记，卸载时不删。
fn claim(d: &Path) -> bool {
    let ours = d.join(MARKER).exists() || fs::read_dir(d).map_or(true, |mut r| r.next().is_none());
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

/// 旧版本的配置（config.json、mpv.conf）在 `%APPDATA%\<id>`，日志、界面缓存、mpv 缓存在 `%LOCALAPPDATA%\<id>`。
/// 新位置还没有 config.json 时把那两个文件搬过来（可能跨盘，所以复制），都成功后删掉 `old_dirs`
/// （包含数据根目录的那个除外）。config.json 最后复制：它出现在新位置就代表搬完了。返回是否搬了。
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

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    /// 本机设备 id，所有服务器共用；首次启动生成。
    pub device_id: String,
    pub active: Option<String>,
    pub servers: Vec<Server>,
    /// mpv 可执行文件；为空时用资源目录自带的，再退回 PATH。
    pub mpv_path: Option<String>,
    /// 当前集播放到这个百分比时预加载下一集。
    pub preload_percent: u8,
    /// 上次退出播放器时的音量，下次起播沿用。
    pub volume: Option<f64>,
    /// 检查更新、下载安装包用的代理（如 `http://127.0.0.1:10808`）；为空时跟随系统代理。其它请求一律不走代理。
    pub update_proxy: Option<String>,
    /// 发送诊断数据（默认关）。
    pub telemetry: bool,
    /// 诊断数据里的「上报名称」，方便区分是谁；为空时用计算机名。
    pub telemetry_name: Option<String>,
    /// 边播边缓存整个视频（默认开）；关掉时缓存代理只缓存播放位置前方一段。
    pub cache_whole: bool,
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
            telemetry: false,
            telemetry_name: None,
            cache_whole: true,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Server {
    pub id: String,
    pub name: String,
    /// `scheme://host[/子路径]`，不含端口，见 [`normalize`]。
    pub url: String,
    pub port: u16,
    pub user: String,
    #[serde(default)]
    pub user_id: String,
    /// 空串表示需要重新登录。只存 token，不存密码。
    #[serde(default)]
    pub token: String,
}

/// 给前端看的服务器信息（不含 token）。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerView {
    pub id: String,
    pub name: String,
    pub url: String,
    pub port: u16,
    pub user: String,
    pub user_id: String,
    /// API 根，前端用它拼图片地址（图片接口不需要 token）。
    pub base: String,
    /// 上游标识（[`server_tag`]），诊断数据按它区分上游。
    pub tag: String,
    pub logged_in: bool,
}

impl Server {
    /// API 根：`scheme://host:port[/子路径]/emby`。
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
            tag: server_tag(&self.api_base()),
            logged_in: !self.token.is_empty(),
        }
    }
}

/// 上游标识：API 根地址的 FNV-1a 哈希取 8 位十六进制。诊断数据靠它把不同上游、同一上游的不同线路
/// （不同中继地址）分开统计，又不把地址本身发出去；知道地址的人算一遍就能对上。算法不能改，否则新旧数据对不上。
pub fn server_tag(api_base: &str) -> String {
    let h = api_base.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{:08x}", h >> 32)
}

/// 把用户输入的地址和端口规范化成 `(scheme://host[/子路径], 端口)`。
///
/// 接受：裸域名、带协议、地址里带端口、从浏览器复制来的 `…/web/index.html#!/home`、带 `/emby` 后缀的地址。
/// 端口优先级：端口栏 > 地址里的端口 > 协议默认端口。
/// 不写协议时按 https；但端口（端口栏或地址里写的）是 80 / 8096（Emby 的 http 默认端口）时按 http。
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
        // 只换协议：地址里写的端口 Url 会保留
        u = Url::parse(&format!("http://{s}")).map_err(|_| format!("地址格式不对：{s}"))?;
    }
    if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none_or(str::is_empty) {
        return Err(format!("只支持 http / https 地址：{s}"));
    }
    let port = port
        .or(u.port())
        .or(u.port_or_known_default())
        .ok_or_else(|| format!("无法确定端口：{s}"))?;
    // 子路径只保留到 /web 或 /emby 之前（反代可能把 Emby 挂在子路径下）。
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

pub fn load(path: &Path) -> Config {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            log::warn!("配置文件损坏，改名为 config.json.bad 后重建：{e}");
            let _ = fs::rename(path, path.with_extension("json.bad"));
            Config::default()
        }),
        Err(_) => Config::default(),
    }
}

/// 先写临时文件再改名，写到一半断电也不会把配置写坏。
pub fn save(path: &Path, cfg: &Config) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(cfg)?)?;
    fs::rename(tmp, path)
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
        // 局域网常见写法：端口写在地址里、不写协议
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
    }

    #[test]
    fn server_tag_is_stable() {
        // 固定值：改了算法，历史诊断数据就和新数据对不上
        assert_eq!(server_tag(""), "cbf29ce4");
        assert_eq!(
            server_tag("https://h.example/emby"),
            server_tag("https://h.example/emby")
        );
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
        assert_eq!(load(&path).device_id, c.device_id);
        fs::write(&path, b"{not json").unwrap();
        assert_eq!(load(&path).preload_percent, 80);
        assert!(path.with_extension("json.bad").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    /// 只给新建 / 空的 / 本来就是我们的目录打卸载标记；别人的 data 照用但不打。
    #[test]
    fn claim_marks_only_our_dirs() {
        let t = std::env::temp_dir().join(format!("embymini-claim-{}", std::process::id()));
        let (fresh, foreign) = (t.join("fresh"), t.join("foreign"));
        assert!(claim(&fresh) && fresh.join(MARKER).exists());
        assert!(claim(&fresh), "第二次启动照常可用");
        fs::create_dir_all(&foreign).unwrap();
        fs::write(foreign.join("别人的文件"), b"x").unwrap();
        assert!(claim(&foreign));
        assert!(!foreign.join(MARKER).exists());
        assert_eq!(fs::read_dir(&foreign).unwrap().count(), 1, "探测文件要删掉");
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
        // 新位置已有配置：什么都不做
        fs::create_dir_all(&old_cfg).unwrap();
        fs::write(old_cfg.join("config.json"), b"{}").unwrap();
        assert!(!migrate(&root, &old_cfg, &old).unwrap());
        assert!(old_cfg.exists());
        // 退回旧的 LOCALAPPDATA 目录当数据根时，不能把它自己删了
        let root2 = old_local.join("data");
        fs::create_dir_all(&old_local).unwrap();
        assert!(migrate(&root2, &old_cfg, &[old_cfg.clone(), old_local.clone()]).unwrap());
        assert!(root2.join("config.json").is_file());
        fs::remove_dir_all(t).unwrap();
    }
}
