//! 与具体 OS 打交道的一薄层：XDG 目录、窗口状态持久化。
//!
//! 这里刻意不做抽象工厂 —— 只有 Linux 目标（`docs/architecture.md` 的范围），
//! 需要跨平台时再抽 trait 比现在就抽便宜。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub mod instance;

use serde::{Deserialize, Serialize};

/// 应用名，用作目录名与桌面文件名。
pub const APP_ID: &str = "dsh-desktop-rs";

static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// `$XDG_STATE_HOME/dsh-desktop-rs`，回退到 `~/.local/state/dsh-desktop-rs`。
#[must_use]
pub fn state_dir() -> PathBuf {
    base_dir("XDG_STATE_HOME", ".local/state").join(APP_ID)
}

/// `$XDG_RUNTIME_DIR/dsh-desktop-rs`，回退到 `$XDG_STATE_HOME/dsh-desktop-rs/run`。
#[must_use]
pub fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join(APP_ID),
        _ => state_dir().join("run"),
    }
}

/// `$XDG_DATA_HOME/dsh-desktop-rs`，回退到 `~/.local/share/dsh-desktop-rs`。
///
/// 安装出来的 node 运行时与 dsh 放这里：它们是「数据」而不是「状态」，
/// 删掉状态目录（窗口状态、配置）不该把装好的运行时一起带走。
///
/// **第一次调用时的值会被记住**（见 [`pin_paths`]）：webview 隔离会把
/// `XDG_DATA_HOME` 指到 tmpfs，之后再来算就会把 node / dsh 装进一重启就没的目录。
#[must_use]
pub fn data_dir() -> PathBuf {
    pinned(&DATA_DIR, "XDG_DATA_HOME", ".local/share")
}

/// 启动最早期调一次，把数据目录钉在「用户真正的主目录」上。
///
/// 顺序很重要：必须在 `isolate_webview_storage()`（改 `XDG_DATA_HOME`）之前。
pub fn pin_paths() {
    let _ = data_dir();
}

fn pinned(cell: &'static OnceLock<PathBuf>, var: &str, fallback: &str) -> PathBuf {
    cell.get_or_init(|| base_dir(var, fallback).join(APP_ID))
        .clone()
}

/// `$XDG_CACHE_HOME/dsh-desktop-rs`，回退到 `~/.cache/dsh-desktop-rs`。
#[must_use]
pub fn cache_dir() -> PathBuf {
    base_dir("XDG_CACHE_HOME", ".cache").join(APP_ID)
}

fn base_dir(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home_dir().join(fallback),
    }
}

/// 用户主目录：`$HOME`，Windows 上 `$HOME` 通常不存在、要看 `%USERPROFILE%`。
///
/// 只认 `$HOME` 的话 Windows 会回退到 `.`（相对当前目录）——于是 settings.json、
/// 日志、装出来的 node 全都落进 exe 所在目录或 `C:\Windows\System32`，看起来像
/// 「配置丢了 / 插件装不上」。所以 Windows 上再补一次 `USERPROFILE`。
#[must_use]
pub fn home_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(dir);
    }
    #[cfg(windows)]
    if let Some(dir) = std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()) {
        return PathBuf::from(dir);
    }
    PathBuf::from(".")
}

/// 上次退出时的窗口几何。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowState {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub maximized: bool,
}

impl Default for WindowState {
    fn default() -> Self {
        // 与 architecture.md §8.1 的默认窗口一致。
        Self {
            x: 80,
            y: 60,
            width: 1440,
            height: 900,
            maximized: false,
        }
    }
}

impl WindowState {
    /// 尺寸夹在合理下限，避免用户把窗口拖成一条线之后再也点不回来。
    #[must_use]
    pub fn sanitized(mut self) -> Self {
        self.width = self.width.clamp(900, 12_000);
        self.height = self.height.clamp(600, 12_000);
        self
    }
}

fn window_state_path() -> PathBuf {
    state_dir().join("window.json")
}

/// 读取上次的窗口状态；文件缺失或损坏都返回 `None`（损坏时顺手删掉）。
#[must_use]
pub fn load_window_state() -> Option<WindowState> {
    let path = window_state_path();
    let raw = fs::read_to_string(&path).ok()?;
    match serde_json::from_str::<WindowState>(&raw) {
        Ok(state) => Some(state.sanitized()),
        Err(_) => {
            let _ = fs::remove_file(&path);
            None
        }
    }
}

/// 原子写入窗口状态（`0600`，先写临时文件再 rename）。
pub fn save_window_state(state: &WindowState) -> io::Result<()> {
    let dir = state_dir();
    fs::create_dir_all(&dir)?;
    let tmp = dir.join("window.json.tmp");
    let body = serde_json::to_vec_pretty(state)?;
    fs::write(&tmp, body)?;
    set_private(&tmp)?;
    fs::rename(&tmp, window_state_path())
}

#[cfg(unix)]
fn set_private(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn pinned_dir_ignores_later_env_changes() {
        static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        let var = "DSH_TEST_XDG_DATA";
        // SAFETY: 这个变量名只在本测试里用，且是进程内唯一读写者。
        unsafe { std::env::set_var(var, "/first/data") };
        let first = super::pinned(&DIR, var, ".local/share");
        assert!(first.starts_with("/first/data"));
        unsafe { std::env::set_var(var, "/second/data") };
        let again = super::pinned(&DIR, var, ".local/share");
        assert_eq!(first, again, "钉住之后不该跟着环境变量漂");
        unsafe { std::env::remove_var(var) };
    }

    use super::*;

    #[test]
    fn window_state_round_trips_and_is_private() {
        let dir = std::env::temp_dir().join(format!("dsh-platform-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // SAFETY: 测试进程内串行使用，且只影响本 crate 的路径解析。
        unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };

        let state = WindowState {
            x: 12,
            y: 34,
            width: 1000,
            height: 700,
            maximized: true,
        };
        save_window_state(&state).unwrap();
        assert_eq!(load_window_state(), Some(state));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(dir.join(APP_ID).join("window.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn sanitize_clamps_absurd_sizes() {
        let s = WindowState {
            x: 0,
            y: 0,
            width: 10,
            height: 100_000,
            maximized: false,
        }
        .sanitized();
        assert_eq!(s.width, 900);
        assert_eq!(s.height, 12_000);
    }
}
