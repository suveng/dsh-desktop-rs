//! 更新提示（`architecture.md` §7-7）。
//!
//! 壳自己不认识「最新版是什么」：它读一个**更新源**（JSON），比对版本，
//! 然后把结果推给前端与菜单。没有配置更新源时状态是 `idle`，不做任何网络请求 ——
//! 与其假装检查过，不如什么都不做。
//!
//! 用 `curl` 而不是引入 TLS 栈：壳的职责是「一次 GET 一个 JSON」，
//! 为此背一个 rustls + hyper 会让安装包翻倍。缺 `curl` 时降级为 `error` 状态并在日志里说明。

use std::process::{Command, Stdio};

/// 与前端 `DesktopUpdatePresentation` 对齐的阶段名。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub phase: &'static str,
    pub version: Option<String>,
    pub url: Option<String>,
    pub failure: Option<String>,
}

impl Status {
    #[must_use]
    pub fn idle() -> Self {
        Self {
            phase: "idle",
            version: None,
            url: None,
            failure: None,
        }
    }

    /// 前端就绪时拿到的首帧状态（shim 的 `updates.status()`）。
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut parts = vec![format!("\"phase\":\"{}\"", self.phase)];
        if let Some(version) = &self.version {
            parts.push(format!("\"version\":\"{version}\""));
        }
        if let Some(url) = &self.url {
            parts.push(format!("\"url\":\"{url}\""));
        }
        if let Some(failure) = &self.failure {
            parts.push(format!("\"failure\":\"{failure}\""));
        }
        format!("{{{}}}", parts.join(","))
    }
}

/// 更新源返回的清单。
#[derive(Debug, PartialEq, Eq)]
pub struct Manifest {
    pub version: String,
    pub url: String,
}

/// 拉取并判断：有新版本返回 `available`，否则 `up-to-date`。
#[must_use]
pub fn check(feed: Option<&str>, current: &str) -> Status {
    let Some(feed) = feed else {
        return Status::idle();
    };

    let body = match fetch(feed) {
        Ok(body) => body,
        Err(err) => {
            return Status {
                phase: "error",
                version: None,
                url: None,
                failure: Some(err),
            };
        }
    };

    let manifest = match parse(&body) {
        Ok(manifest) => manifest,
        Err(err) => {
            return Status {
                phase: "error",
                version: None,
                url: None,
                failure: Some(err),
            };
        }
    };

    let newer = is_newer(&manifest.version, current);
    Status {
        phase: if newer { "available" } else { "up-to-date" },
        version: Some(manifest.version),
        url: Some(manifest.url),
        failure: None,
    }
}

fn fetch(url: &str) -> Result<String, String> {
    let mut cmd = Command::new("curl");
    dsh_platform::quiet(&mut cmd);
    let output = cmd
        .args(["-fsSL", "--max-time", "10", url])
        .stdin(Stdio::null())
        .output()
        .map_err(|err| format!("无法执行 curl：{err}"))?;
    if !output.status.success() {
        return Err(format!(
            "更新源返回 HTTP {}",
            output.status.code().unwrap_or(-1)
        ));
    }
    String::from_utf8(output.stdout).map_err(|_| "更新源返回了非 UTF-8 内容".to_owned())
}

fn parse(body: &str) -> Result<Manifest, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|err| format!("更新源不是合法 JSON：{err}"))?;
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "更新源缺少 version".to_owned())?;
    let url = value
        .get("url")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "更新源缺少 url".to_owned())?;
    Ok(Manifest {
        version: version.to_owned(),
        url: url.to_owned(),
    })
}

/// 只比较点分数字段，够用且不会误判；预发布后缀按「同数字时更小」处理。
#[must_use]
pub fn is_newer(candidate: &str, current: &str) -> bool {
    fn split(v: &str) -> (Vec<u64>, bool) {
        let (core, pre) = match v.split_once('-') {
            Some((core, _)) => (core, true),
            None => (v, false),
        };
        (
            core.split('.')
                .map(|part| part.parse::<u64>().unwrap_or(0))
                .collect(),
            pre,
        )
    }

    let (a, a_pre) = split(candidate);
    let (b, b_pre) = split(current);
    let len = a.len().max(b.len());
    for index in 0..len {
        let left = a.get(index).copied().unwrap_or(0);
        let right = b.get(index).copied().unwrap_or(0);
        match left.cmp(&right) {
            std::cmp::Ordering::Greater => return true,
            std::cmp::Ordering::Less => return false,
            std::cmp::Ordering::Equal => {}
        }
    }
    // 数字完全相同：正式版比预发布版新。
    b_pre && !a_pre
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_versions_are_detected() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
    }

    #[test]
    fn prerelease_is_older_than_release() {
        assert!(is_newer("0.1.0", "0.1.0-rc.1"));
        assert!(!is_newer("0.1.0-rc.1", "0.1.0"));
    }

    #[test]
    fn manifest_requires_version_and_url() {
        let ok = parse(r#"{"version":"0.2.0","url":"https://example.test/x.deb"}"#).unwrap();
        assert_eq!(ok.version, "0.2.0");
        assert!(parse(r#"{"version":"0.2.0"}"#).is_err());
        assert!(parse("not json").is_err());
    }

    #[test]
    fn missing_feed_is_idle_and_makes_no_request() {
        let status = check(None, "0.1.0");
        assert_eq!(status.phase, "idle");
        assert!(status.failure.is_none());
    }
}
