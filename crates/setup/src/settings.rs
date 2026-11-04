//! 引导向导的配置：代理、npm 镜像、版本偏好、选定 profile、安装历史。
//!
//! 存放位置 `$XDG_STATE_HOME/dsh-desktop-rs/settings.json`，权限 `0600`（可能含代理凭据）。
//! 读取永不失败：文件缺失或损坏都回落默认值 —— 配置坏了不该让软件起不来。

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 支持区间下界（与 `docs/architecture.md` D7 一致）。
pub const MIN_HOST_VERSION: &str = "0.2.2-alpha.2";
/// 支持区间上界（不含）。
pub const MAX_HOST_VERSION: &str = "0.3.0";
/// 默认安装版本：落在支持区间内、且是当前 alpha 线的最新版。
pub const DEFAULT_DSH_VERSION: &str = "0.2.2-alpha.3";
/// 默认启动的 profile。
pub const DEFAULT_PROFILE: &str = "web";
/// 没有联网拿到 LTS 清单时的兜底 node 版本。
pub const FALLBACK_NODE_VERSION: &str = "24.13.0";
/// dsh 要求的 node 下界（`apps/cli/package.json` 的 engines）。
pub const MIN_NODE_VERSION: &str = "24.9.0";

/// 安装落点。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallScope {
    /// 先试全局 npm 前缀；不可写（EACCES）时自动落用户前缀。
    #[default]
    Auto,
    /// 只装全局。
    Global,
    /// 只装用户前缀（`~/.local/share/dsh-desktop-rs/host`）。
    User,
}

/// 安装子进程与后续宿主进程共用的代理。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Proxy {
    pub http: String,
    pub https: String,
    pub all: String,
    pub no_proxy: String,
}

impl Proxy {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.http.is_empty() && self.https.is_empty() && self.all.is_empty()
    }

    /// 注入子进程的环境变量。npm 认小写，curl 认大写，两套都写。
    #[must_use]
    pub fn env(&self) -> Vec<(String, String)> {
        let mut env = Vec::new();
        for (value, names) in [
            (&self.http, ["HTTP_PROXY", "http_proxy"]),
            (&self.https, ["HTTPS_PROXY", "https_proxy"]),
            (&self.all, ["ALL_PROXY", "all_proxy"]),
            (&self.no_proxy, ["NO_PROXY", "no_proxy"]),
        ] {
            if value.is_empty() {
                continue;
            }
            for name in names {
                env.push((name.to_owned(), value.clone()));
            }
        }
        env
    }

    /// `http://user:pass@host:3128` → `http://***@host:3128`，用于日志与界面回显。
    #[must_use]
    pub fn redacted(&self) -> Self {
        Self {
            http: redact_url(&self.http),
            https: redact_url(&self.https),
            all: redact_url(&self.all),
            no_proxy: self.no_proxy.clone(),
        }
    }
}

/// 把 URL 里的凭据段打掉。不是 URL 就原样返回。
#[must_use]
pub fn redact_url(raw: &str) -> String {
    let Some((scheme, rest)) = raw.split_once("://") else {
        return raw.to_owned();
    };
    match rest.split_once('@') {
        Some((_credentials, host)) => format!("{scheme}://***@{host}"),
        None => raw.to_owned(),
    }
}

/// 一次成功的安装记录，用于「回退到上一个版本」。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledVersion {
    pub version: String,
    /// 装到了哪里（可执行文件绝对路径），回退时优先复用同一落点。
    pub program: String,
    /// Unix 秒。排序用，不追求可读。
    pub installed_at: u64,
}

/// `settings.json` 的形状。所有字段都有默认值，旧文件缺字段也能读。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub proxy: Proxy,
    /// 空字符串 = 用 npm 自身的 registry（`~/.npmrc`）。非空时作为 `npm_config_registry`。
    pub npm_registry: String,
    /// 想装的 dsh 版本；空 = 用 [`DEFAULT_DSH_VERSION`]。
    pub dsh_version: String,
    /// 想装的 node 版本；空 = 跟随 LTS。
    pub node_version: String,
    pub install_scope: InstallScope,
    /// 启动时用的 profile 名。
    pub profile: String,
    /// 已成功安装过的版本，最新的在前。
    pub history: Vec<InstalledVersion>,
    /// 额外注入宿主 `PATH` 的目录，支持 `~`。排在自动探测的目录之后。
    pub extra_path: Vec<String>,
    /// 额外注入宿主的环境变量（`KEY` → `VALUE`），同名项覆盖进程环境。
    pub env: BTreeMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            proxy: Proxy::default(),
            npm_registry: String::new(),
            dsh_version: String::new(),
            node_version: String::new(),
            install_scope: InstallScope::default(),
            profile: DEFAULT_PROFILE.to_owned(),
            history: Vec::new(),
            extra_path: Vec::new(),
            env: BTreeMap::new(),
        }
    }
}

impl Settings {
    /// 目标 dsh 版本（空值回落默认）。
    #[must_use]
    pub fn target_dsh_version(&self) -> &str {
        if self.dsh_version.is_empty() {
            DEFAULT_DSH_VERSION
        } else {
            &self.dsh_version
        }
    }

    /// 启动时用的 profile（空值回落 `web`）。
    #[must_use]
    pub fn target_profile(&self) -> &str {
        if self.profile.is_empty() {
            DEFAULT_PROFILE
        } else {
            &self.profile
        }
    }

    /// 宿主进程要带的环境变量：用户在设置里填的那些。
    #[must_use]
    pub fn host_env(&self) -> Vec<(String, String)> {
        self.env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }

    /// `extra_path` 展开 `~` 后的目录列表（空串与空白项丢弃）。
    #[must_use]
    pub fn extra_path_dirs(&self, home: &std::path::Path) -> Vec<std::path::PathBuf> {
        self.extra_path
            .iter()
            .map(|entry| entry.trim())
            .filter(|entry| !entry.is_empty())
            .map(|entry| match entry.strip_prefix("~/") {
                Some(rest) => home.join(rest),
                None if entry == "~" => home.to_path_buf(),
                None => std::path::PathBuf::from(entry),
            })
            .collect()
    }

    /// 安装子进程要带的环境变量：代理 + registry。
    #[must_use]
    pub fn install_env(&self) -> Vec<(String, String)> {
        let mut env = self.proxy.env();
        if !self.npm_registry.is_empty() {
            env.push(("npm_config_registry".to_owned(), self.npm_registry.clone()));
        }
        env
    }

    /// 记一次安装。同版本的旧记录被顶掉，避免历史里堆重复项。
    pub fn record_install(&mut self, version: &str, program: &str, at: u64) {
        self.history.retain(|entry| entry.version != version);
        self.history.insert(
            0,
            InstalledVersion {
                version: version.to_owned(),
                program: program.to_owned(),
                installed_at: at,
            },
        );
        // 只留最近 10 条：回退用不了那么多。
        self.history.truncate(10);
    }

    /// 上一个装过的、与当前版本不同的版本（「回退」的目标）。
    #[must_use]
    pub fn previous_version(&self, current: Option<&str>) -> Option<&InstalledVersion> {
        self.history
            .iter()
            .find(|entry| Some(entry.version.as_str()) != current)
    }

    /// 用于界面与日志的回显：代理凭据打掉。
    #[must_use]
    pub fn redacted(&self) -> Self {
        let mut clone = self.clone();
        clone.proxy = self.proxy.redacted();
        clone
    }
}

/// `settings.json` 在状态目录里的文件名。
pub const SETTINGS_FILE: &str = "settings.json";

#[must_use]
pub fn settings_path(state_dir: &Path) -> PathBuf {
    state_dir.join(SETTINGS_FILE)
}

/// 读配置。文件缺失、权限不足、JSON 坏掉一律返回默认值。
#[must_use]
pub fn load(state_dir: &Path) -> Settings {
    let path = settings_path(state_dir);
    let Ok(raw) = fs::read_to_string(&path) else {
        return Settings::default();
    };
    match serde_json::from_str(&raw) {
        Ok(settings) => settings,
        Err(err) => {
            eprintln!(
                "dsh-desktop: {} 解析失败（{err}），使用默认配置",
                path.display()
            );
            Settings::default()
        }
    }
}

/// 原子写 + `0600`。先写临时文件再 rename，避免半截文件。
pub fn save(state_dir: &Path, settings: &Settings) -> io::Result<()> {
    ensure_private_dir(state_dir)?;
    let body = serde_json::to_string_pretty(settings)?;
    let tmp = state_dir.join(format!("{SETTINGS_FILE}.tmp"));
    {
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(body.as_bytes())?;
        file.flush()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(&tmp, settings_path(state_dir))
}

/// 状态目录本身也收紧到 `0700`：里面的备份与配置都不该被别的用户读。
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_env_defaults_are_empty() {
        let settings = Settings::default();
        assert!(settings.extra_path.is_empty());
        assert!(settings.env.is_empty());
        assert!(settings.host_env().is_empty());
    }

    #[test]
    fn extra_path_dirs_expand_tilde_and_skip_blanks() {
        let home = PathBuf::from("/Users/example");
        let settings = Settings {
            extra_path: vec![
                "  ".to_owned(),
                "~/bin".to_owned(),
                "~".to_owned(),
                "/opt/tool/bin".to_owned(),
            ],
            ..Default::default()
        };
        assert_eq!(
            settings.extra_path_dirs(&home),
            vec![
                PathBuf::from("/Users/example/bin"),
                PathBuf::from("/Users/example"),
                PathBuf::from("/opt/tool/bin"),
            ]
        );
    }

    #[test]
    fn host_env_round_trips_through_json() {
        let mut settings = Settings {
            extra_path: vec!["~/bin".to_owned()],
            ..Default::default()
        };
        settings
            .env
            .insert("HTTPS_PROXY".to_owned(), "http://127.0.0.1:7890".to_owned());
        let text = serde_json::to_string(&settings).unwrap();
        let back: Settings = serde_json::from_str(&text).unwrap();
        assert_eq!(back, settings);
        assert_eq!(
            back.host_env(),
            vec![("HTTPS_PROXY".to_owned(), "http://127.0.0.1:7890".to_owned())]
        );
    }

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dsh-setup-settings-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn save_writes_private_file() {
        let dir = tempdir("perm");
        let mut settings = Settings::default();
        settings.proxy.https = "http://user:secret@127.0.0.1:7890".to_owned();
        save(&dir, &settings).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(settings_path(&dir))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "settings.json 必须是 0600");
        }
        assert_eq!(load(&dir), settings, "写进去的要能读回来");
    }

    #[test]
    fn missing_or_broken_file_falls_back_to_defaults() {
        let dir = tempdir("broken");
        assert_eq!(load(&dir), Settings::default());
        fs::write(settings_path(&dir), "{ not json").unwrap();
        assert_eq!(load(&dir), Settings::default());
    }

    #[test]
    fn partial_file_keeps_defaults_for_missing_keys() {
        let dir = tempdir("partial");
        fs::write(
            settings_path(&dir),
            r#"{"profile":"open-design","proxy":{"https":"http://p:1"}}"#,
        )
        .unwrap();
        let settings = load(&dir);
        assert_eq!(settings.profile, "open-design");
        assert_eq!(settings.proxy.https, "http://p:1");
        assert_eq!(settings.install_scope, InstallScope::Auto);
        assert!(settings.history.is_empty());
    }

    #[test]
    fn display_copy_redacts_credentials() {
        let mut settings = Settings::default();
        settings.proxy.http = "http://alice:token@proxy.local:7890".to_owned();
        settings.proxy.no_proxy = "localhost".to_owned();
        let shown = settings.redacted();
        assert_eq!(shown.proxy.http, "http://***@proxy.local:7890");
        assert_eq!(shown.proxy.no_proxy, "localhost");
        // 原对象不受影响：真正写盘时仍要带凭据。
        assert_eq!(settings.proxy.http, "http://alice:token@proxy.local:7890");
    }

    #[test]
    fn install_env_maps_proxy_and_registry() {
        let mut settings = Settings::default();
        settings.proxy.https = "http://127.0.0.1:7890".to_owned();
        settings.npm_registry = "https://registry.npmmirror.com".to_owned();
        let env = settings.install_env();
        let has = |key: &str, value: &str| env.iter().any(|(k, v)| k == key && v == value);
        assert!(has("HTTPS_PROXY", "http://127.0.0.1:7890"));
        assert!(has("https_proxy", "http://127.0.0.1:7890"));
        assert!(has("npm_config_registry", "https://registry.npmmirror.com"));
        // 没填的代理变量不该凭空出现。
        assert!(!env.iter().any(|(k, _)| k == "HTTP_PROXY"));
    }

    #[test]
    fn history_dedupes_and_orders_newest_first() {
        let mut settings = Settings::default();
        settings.record_install("0.2.1-rc.5", "/usr/bin/dsh", 1);
        settings.record_install("0.2.2-alpha.3", "/home/u/.local/bin/dsh", 2);
        settings.record_install("0.2.1-rc.5", "/usr/bin/dsh", 3);

        let versions: Vec<&str> = settings
            .history
            .iter()
            .map(|entry| entry.version.as_str())
            .collect();
        assert_eq!(versions, vec!["0.2.1-rc.5", "0.2.2-alpha.3"]);

        assert_eq!(
            settings
                .previous_version(Some("0.2.1-rc.5"))
                .map(|e| e.version.as_str()),
            Some("0.2.2-alpha.3")
        );
        assert_eq!(settings.target_dsh_version(), DEFAULT_DSH_VERSION);
    }

    #[test]
    fn target_profile_falls_back_to_web() {
        let mut settings = Settings::default();
        settings.profile.clear();
        assert_eq!(settings.target_profile(), "web");
        settings.profile = "open-design".to_owned();
        assert_eq!(settings.target_profile(), "open-design");
    }
}
