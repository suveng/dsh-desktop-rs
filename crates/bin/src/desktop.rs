//! 桌面集成：托盘、系统通知、全局热键、拖拽真实路径。
//!
//! 四项都遵循同一条原则（见 M0 的教训）：**能探测就先探测，不能用的功能安静降级**，
//! 绝不在用户机器上假装自己工作。每项都在日志里留下一行「为什么没启用」。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// 托盘菜单项 id（与 `menu::Action` 分开：托盘只暴露三个动作）。
pub const TRAY_SHOW: &str = "tray-show";
pub const TRAY_RELOAD: &str = "tray-reload";
pub const TRAY_QUIT: &str = "tray-quit";

/// GNOME 默认没有 StatusNotifierWatcher，托盘图标会「创建成功但看不见」。
/// 与其让用户以为坏了，不如先问一句 D-Bus。
#[must_use]
pub fn status_notifier_available() -> bool {
    let output = Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.freedesktop.DBus",
            "--object-path",
            "/org/freedesktop/DBus",
            "--method",
            "org.freedesktop.DBus.NameHasOwner",
            "org.kde.StatusNotifierWatcher",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();

    match output {
        Ok(out) => String::from_utf8_lossy(&out.stdout).contains("true"),
        Err(_) => false,
    }
}

/// 托盘图标。只有探测通过才会被创建。
pub struct Tray {
    _icon: TrayIcon,
}

impl Tray {
    /// 创建托盘；返回 `(tray, 是否真的可用)`。
    ///
    /// 不可用时返回 `None`，调用方据此在日志里说明原因，而不是静默消失。
    pub fn create() -> Option<Self> {
        if !status_notifier_available() {
            eprintln!(
                "dsh-desktop: 托盘未启用 —— 当前会话没有 org.kde.StatusNotifierWatcher（GNOME 需要 AppIndicator 扩展）"
            );
            return None;
        }

        let menu = Menu::new();
        for (id, label) in [
            (TRAY_SHOW, "显示窗口"),
            (TRAY_RELOAD, "重新加载界面"),
            (TRAY_QUIT, "退出"),
        ] {
            let item = MenuItem::with_id(id, label, true, None);
            menu.append(&item).ok()?;
        }
        menu.append(&PredefinedMenuItem::separator()).ok()?;

        let icon = Icon::from_rgba(tray_icon_rgba(), 32, 32).ok()?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("DeepSeek Harness")
            .with_icon(icon)
            .build()
            .ok()?;

        Some(Self { _icon: tray })
    }

    /// 取走托盘菜单的点击（每帧调一次，返回 `show` / `reload` / `quit`）。
    #[must_use]
    pub fn drain(&self) -> Vec<&'static str> {
        let receiver = MenuEvent::receiver();
        let mut actions = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            for id in [TRAY_SHOW, TRAY_RELOAD, TRAY_QUIT] {
                if event.id.0 == id {
                    actions.push(id);
                }
            }
        }
        actions
    }
}

/// 系统通知。`notify-rust` 走 D-Bus；没有通知服务时返回 `false` 并只记一行日志。
pub fn notify(title: &str, body: &str) -> bool {
    match notify_rust::Notification::new()
        .summary(title)
        .body(body)
        .appname("dsh-desktop")
        .timeout(notify_rust::Timeout::Milliseconds(6000))
        .show()
    {
        Ok(_) => true,
        Err(err) => {
            eprintln!("dsh-desktop: 系统通知不可用：{err}");
            false
        }
    }
}

/// 全局热键：`Ctrl+Alt+D` 唤回窗口。
pub struct HotKeys {
    _manager: global_hotkey::GlobalHotKeyManager,
    id: global_hotkey::hotkey::HotKey,
}

impl HotKeys {
    pub fn register() -> Option<Self> {
        use global_hotkey::hotkey::{Code, HotKey, Modifiers};

        let manager = global_hotkey::GlobalHotKeyManager::new().ok()?;
        let id = HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyD);
        if let Err(err) = manager.register(id) {
            eprintln!("dsh-desktop: 全局热键 Ctrl+Alt+D 注册失败：{err}");
            return None;
        }
        eprintln!("dsh-desktop: 全局热键 Ctrl+Alt+D 已注册");
        Some(Self {
            _manager: manager,
            id,
        })
    }

    /// 本帧是否收到我们注册的热键。
    #[must_use]
    pub fn triggered(&self) -> bool {
        let receiver = global_hotkey::GlobalHotKeyEvent::receiver();
        let mut hit = false;
        while let Ok(event) = receiver.try_recv() {
            if event.id == self.id.id() {
                hit = true;
            }
        }
        hit
    }
}

/// 拖拽进来的真实路径（`architecture.md` §7-9 / R2）。
///
/// WebKitGTK 的 `File` 没有 `.path`，但 wry 的原生拖拽事件带着完整路径。
/// 因此：原生侧记下这一批路径，页面调 `pathFor(file)` 时按**文件名**取回。
/// 同名文件取第一个匹配，取过即出队，避免连续拖同名文件时串位。
#[derive(Default)]
pub struct DropPaths {
    queue: Mutex<VecDeque<PathBuf>>,
}

impl DropPaths {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一次原生 drop。
    pub fn record<I, P>(&self, paths: I)
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        if let Ok(mut queue) = self.queue.lock() {
            queue.clear();
            queue.extend(paths.into_iter().map(Into::into));
        }
    }

    /// 按文件名取路径；找不到返回 `None`（页面会退回「上传字节」）。
    #[must_use]
    pub fn resolve(&self, name: &str) -> Option<String> {
        if name.is_empty() {
            return None;
        }
        let mut queue = self.queue.lock().ok()?;
        let index = queue
            .iter()
            .position(|path| path.file_name().is_some_and(|file| file == name))?;
        queue.remove(index).map(|path| path.display().to_string())
    }

    /// 队列里还有什么（仅用于自检与日志）。
    #[must_use]
    pub fn len(&self) -> usize {
        self.queue.lock().map(|queue| queue.len()).unwrap_or(0)
    }
}

/// 32×32 的托盘图标（RGBA）。用代码画，免得为一张小图引入 PNG 解码器。
fn tray_icon_rgba() -> Vec<u8> {
    const SIZE: usize = 32;
    let mut rgba = Vec::with_capacity(SIZE * SIZE * 4);
    let center = SIZE as f32 / 2.0;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let distance = (dx * dx + dy * dy).sqrt();
            // 一个环形：半径 9~13 之间点亮，其余透明。
            let on_ring = (9.0..=13.0).contains(&distance);
            let core = distance < 3.5;
            if on_ring || core {
                rgba.extend_from_slice(&[124, 196, 248, 255]);
            } else {
                rgba.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_paths_resolve_by_file_name_and_consume() {
        let drops = DropPaths::new();
        drops.record(["/tmp/a/report.csv", "/tmp/b/data.json"]);

        assert_eq!(
            drops.resolve("report.csv").as_deref(),
            Some("/tmp/a/report.csv")
        );
        assert_eq!(drops.resolve("report.csv"), None, "取出后不应再命中");
        assert_eq!(
            drops.resolve("data.json").as_deref(),
            Some("/tmp/b/data.json")
        );
        assert_eq!(drops.resolve("missing.txt"), None);
        assert_eq!(drops.len(), 0);
    }

    #[test]
    fn a_new_drop_replaces_the_previous_batch() {
        let drops = DropPaths::new();
        drops.record(["/tmp/old.txt"]);
        drops.record(["/tmp/new.txt"]);
        assert_eq!(drops.resolve("old.txt"), None, "上一批不该残留");
        assert_eq!(drops.resolve("new.txt").as_deref(), Some("/tmp/new.txt"));
    }

    #[test]
    fn empty_name_never_matches() {
        let drops = DropPaths::new();
        drops.record(["/tmp/x"]);
        assert_eq!(drops.resolve(""), None);
    }

    #[test]
    fn tray_icon_buffer_has_expected_shape() {
        assert_eq!(tray_icon_rgba().len(), 32 * 32 * 4);
    }
}
