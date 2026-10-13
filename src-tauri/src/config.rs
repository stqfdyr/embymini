//! 本地配置：服务器列表、当前服务器、播放设置。存成一个 JSON 文件（应用配置目录下的 config.json）。

use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};

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
            logged_in: !self.token.is_empty(),
        }
    }
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
}
