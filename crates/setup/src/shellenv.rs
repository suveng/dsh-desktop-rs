//! 宿主进程的 `PATH`：登录 shell 探测 + 常见包管理器目录兜底。
//!
//! 从 Dock / Finder 启动的 GUI 进程继承的是 launchd 的精简环境，`PATH` 只有
//! `/usr/bin:/bin:/usr/sbin:/sbin`。用户装在 nvm、volta、asdf、`~/.local/bin`
//! 里的 `pnpm` / `node` / `git` 统统不在里面，宿主里的插件管理就会以
//! `spawn pnpm ENOENT` 失败（插件页「无法检查更新」）。
//!
//! 两条来源，先精确后近似：
//!
//! 1. 跑一次用户的登录 shell，把它最终的 `PATH` 读出来（rc 文件里 `nvm use` 的效果都在）；
//! 2. 探测不到（没有 `$SHELL`、shell 卡住、被 [`DISABLE_VAR`] 关掉）时，用
//!    [`common_dirs`] 里那几个约定俗成的目录兜底。
//!
//! 两条都失败也不拦启动：调用方回落到进程自己的 `PATH`。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// 关掉登录 shell 探测：`DSH_DESKTOP_NO_SHELL_PATH=1`。
pub const DISABLE_VAR: &str = "DSH_DESKTOP_NO_SHELL_PATH";

/// 探测超时：登录 shell 会跑 rc（nvm / oh-my-zsh 常见几百毫秒），给余量但必须封顶。
const TIMEOUT: Duration = Duration::from_secs(5);

const MARK: &str = "__DSH_DESKTOP_PATH__";

/// 读登录 shell 的 `PATH`，失败返回 `None`。
///
/// 用 `-ilc` 走交互式登录初始化：nvm 一般只把 `bin` 加在 `.zshrc` / `.bashrc` 里，
/// 只加 `-c` 拿不到。输出用一对标记包住，rc 里的 echo / 提示符不会混进结果。
#[must_use]
pub fn login_shell_path() -> Option<PathBuf> {
    if std::env::var_os(DISABLE_VAR).is_some_and(|value| value != "0") {
        return None;
    }
    let shell = std::env::var_os("SHELL").filter(|value| !value.is_empty())?;
    let script = format!("printf '%s%s%s' '{MARK}' \"$PATH\" '{MARK}'");
    let mut child = Command::new(&shell)
        .args(["-ilc", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }

    let mut out = String::new();
    {
        use std::io::Read as _;
        child.stdout.take()?.read_to_string(&mut out).ok()?;
    }
    let start = out.find(MARK)? + MARK.len();
    let rest = &out[start..];
    let end = rest.find(MARK)?;
    let path = rest[..end].trim();
    if path.is_empty() {
        return None;
    }
    Some(PathBuf::from(path))
}

/// 约定俗成的包管理器目录，按「越可能是用户自己装的越靠前」排序。
///
/// 登录 shell 探测失败时用它兜底。`~/.nvm/versions/node/*/bin` 按版本号从新到旧排，
/// 这样多版本并存时优先用新 node。
#[must_use]
pub fn common_dirs(home: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![
        home.join(".local/bin"),
        home.join(".bun/bin"),
        home.join(".volta/bin"),
        home.join(".asdf/shims"),
        home.join(".cargo/bin"),
    ];
    dirs.extend(nvm_node_bins(home));
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    dirs
}

/// `~/.nvm/versions/node/*/bin`，版本号从新到旧。
#[must_use]
pub fn nvm_node_bins(home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(home.join(".nvm/versions/node")) else {
        return Vec::new();
    };
    let mut versions: Vec<(Vec<u64>, PathBuf)> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let key = version_key(&name);
            (!key.is_empty()).then(|| (key, entry.path().join("bin")))
        })
        .collect();
    versions.sort_by(|left, right| right.0.cmp(&left.0));
    versions.into_iter().map(|(_, dir)| dir).collect()
}

/// `v20.11.1` / `24.13.0` → `[24, 13, 0]`；解析不出返回空（调用方据此丢弃该目录）。
fn version_key(name: &str) -> Vec<u64> {
    name.trim_start_matches('v')
        .split('.')
        .map(|part| part.parse::<u64>().ok())
        .collect::<Option<Vec<u64>>>()
        .unwrap_or_default()
}

/// 把 `extra` 目录按顺序前置到 `base` 前面，去重并保留首次出现的顺序。
#[must_use]
pub fn compose_path(extra: &[PathBuf], base: Option<&std::ffi::OsStr>) -> std::ffi::OsString {
    let mut dirs: Vec<PathBuf> = Vec::with_capacity(extra.len() + 8);
    let mut push = |dir: PathBuf| {
        if !dir.as_os_str().is_empty() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    };
    for dir in extra {
        push(dir.clone());
    }
    if let Some(base) = base {
        for dir in std::env::split_paths(base) {
            push(dir);
        }
    }
    std::env::join_paths(dirs).unwrap_or_default()
}

/// 探测到的 `PATH`：登录 shell 优先，其次常见目录，最后才是进程自己的。
#[must_use]
pub fn detected_path(home: &Path) -> std::ffi::OsString {
    if let Some(from_shell) = login_shell_path() {
        return compose_path(&[], Some(from_shell.as_os_str()));
    }
    compose_path(&common_dirs(home), std::env::var_os("PATH").as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn extra_dirs_come_first_and_duplicates_collapse() {
        let extra = vec![PathBuf::from("/opt/a"), PathBuf::from("/opt/b")];
        let base = OsString::from("/opt/b:/usr/bin:/opt/a");
        let merged = compose_path(&extra, Some(&base));
        let dirs: Vec<_> = std::env::split_paths(&merged).collect();
        assert_eq!(
            dirs,
            vec![
                PathBuf::from("/opt/a"),
                PathBuf::from("/opt/b"),
                PathBuf::from("/usr/bin")
            ]
        );
    }

    #[test]
    fn an_empty_base_keeps_the_extra_dirs() {
        let extra = vec![PathBuf::from("/only")];
        assert_eq!(compose_path(&extra, None), OsString::from("/only"));
    }

    #[test]
    fn empty_entries_are_dropped() {
        let extra = vec![PathBuf::from("")];
        let merged = compose_path(&extra, Some(&OsString::from("/usr/bin")));
        assert_eq!(merged, OsString::from("/usr/bin"));
    }

    #[test]
    fn nvm_bins_are_sorted_newest_first() {
        let home = std::env::temp_dir().join(format!("dsh-nvm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        for version in ["v18.20.0", "v24.13.0", "v20.11.1"] {
            std::fs::create_dir_all(home.join(".nvm/versions/node").join(version).join("bin"))
                .expect("create nvm dir");
        }
        let bins: Vec<_> = nvm_node_bins(&home)
            .into_iter()
            .map(|dir| {
                dir.file_name()
                    .map(|_| dir.parent().unwrap().file_name().unwrap().to_string_lossy().to_string())
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(bins, vec!["v24.13.0", "v20.11.1", "v18.20.0"]);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn common_dirs_include_the_usual_suspects() {
        let home = Path::new("/Users/example");
        let dirs = common_dirs(home);
        assert!(dirs.contains(&home.join(".local/bin")));
        assert!(dirs.contains(&PathBuf::from("/opt/homebrew/bin")));
    }

    #[test]
    fn version_keys_parse_and_reject() {
        assert_eq!(version_key("v24.13.0"), vec![24, 13, 0]);
        assert_eq!(version_key("24.13.0"), vec![24, 13, 0]);
        assert!(version_key("default").is_empty());
    }
}
