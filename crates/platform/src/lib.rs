//! 与具体 OS 打交道的一薄层：XDG 目录、窗口状态持久化。
//!
//! 这里刻意不做抽象工厂 —— 只有 Linux 目标（`docs/architecture.md` 的范围），
//! 需要跨平台时再抽 trait 比现在就抽便宜。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 应用名，用作目录名与桌面文件名。
pub const APP_ID: &str = "dsh-desktop-rs";

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

fn home_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
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
