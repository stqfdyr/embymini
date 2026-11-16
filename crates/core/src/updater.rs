//! The update logic that does not depend on the installer technology: the proxy setting and progress steps.
//! Checking, downloading, verifying and installing come with the new installer.

use crate::emby::Error;

/// Update proxy: `http://`, `https://`, `socks5://` or `socks5h://` plus host and port (or the scheme default).
/// Also the validation when settings are saved.
pub fn proxy_url(p: &str) -> Result<reqwest::Url, Error> {
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

/// Download progress: emitted only when the percent (MB when the size is unknown) changes, not per chunk.
#[derive(Default)]
pub struct Progress {
    got: u64,
    shown: Option<u64>,
}

impl Progress {
    /// Adds a chunk of `n` bytes; returns the bytes so far when the step changed.
    pub fn advance(&mut self, n: usize, total: Option<u64>) -> Option<u64> {
        self.got += n as u64;
        let step = total.map_or(self.got >> 20, |t| self.got * 100 / t.max(1));
        (self.shown.replace(step) != Some(step)).then_some(self.got)
    }
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
}
