//! 原生菜单栏（muda → GTK menubar）。
//!
//! 约定：**快捷键表是唯一真相**。菜单项由这张表生成，加速键也由它注册，
//! 所以「菜单里有的命令一定有快捷键，快捷键一定能从表里查到」
//! （对照 desigma 的反例：快捷键写死在 `match (key, shift, alt)` 里，
//! 结果不可配置、帮助页只能手写）。

use std::collections::HashMap;

use muda::accelerator::{Accelerator, Code, Modifiers};
use muda::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};

/// 壳自己处理的命令。界面内部的快捷键不经过这里。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Reload,
    OpenLogs,
    Quit,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    ToggleFullscreen,
    Minimize,
    About,
    CheckUpdate,
}

impl Action {
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Reload => "reload",
            Self::OpenLogs => "open-logs",
            Self::Quit => "quit",
            Self::ZoomIn => "zoom-in",
            Self::ZoomOut => "zoom-out",
            Self::ZoomReset => "zoom-reset",
            Self::ToggleFullscreen => "fullscreen",
            Self::Minimize => "minimize",
            Self::About => "about",
            Self::CheckUpdate => "check-update",
        }
    }
}

struct Binding {
    action: Action,
    label: &'static str,
    accel: Option<Accelerator>,
    /// 给帮助页看的可读写法。`muda::Accelerator` 没有 `Display`，所以显式存一份。
    hint: &'static str,
}

fn accel(modifiers: Modifiers, code: Code) -> Accelerator {
    Accelerator::new(modifiers, code)
}

/// 菜单结构 + 快捷键表。顺序即菜单顺序。
fn bindings() -> Vec<(&'static str, Vec<Binding>)> {
    use Code as C;
    use Modifiers as M;

    let b = |action, label, accel, hint| Binding {
        action,
        label,
        accel,
        hint,
    };

    vec![
        (
            "文件",
            vec![
                b(
                    Action::Reload,
                    "重新加载界面",
                    Some(accel(M::CONTROL, C::KeyR)),
                    "Ctrl+R",
                ),
                b(Action::OpenLogs, "打开日志目录", None, ""),
                b(
                    Action::Quit,
                    "退出",
                    Some(accel(M::CONTROL, C::KeyQ)),
                    "Ctrl+Q",
                ),
            ],
        ),
        (
            "视图",
            vec![
                b(
                    Action::ZoomIn,
                    "放大",
                    Some(accel(M::CONTROL, C::Equal)),
                    "Ctrl+=",
                ),
                b(
                    Action::ZoomOut,
                    "缩小",
                    Some(accel(M::CONTROL, C::Minus)),
                    "Ctrl+-",
                ),
                b(
                    Action::ZoomReset,
                    "实际大小",
                    Some(accel(M::CONTROL, C::Digit0)),
                    "Ctrl+0",
                ),
                b(
                    Action::ToggleFullscreen,
                    "全屏",
                    Some(accel(M::empty(), C::F11)),
                    "F11",
                ),
            ],
        ),
        (
            "窗口",
            vec![b(
                Action::Minimize,
                "最小化",
                Some(accel(M::CONTROL, C::KeyM)),
                "Ctrl+M",
            )],
        ),
        (
            "帮助",
            vec![
                b(Action::CheckUpdate, "检查更新", None, ""),
                b(Action::About, "关于 dsh-desktop", None, ""),
            ],
        ),
    ]
}

pub struct MenuBar {
    pub menu: Menu,
    by_accel: HashMap<Accelerator, (Action, &'static str, &'static str)>,
    by_id: HashMap<MenuId, Action>,
}

impl MenuBar {
    /// 由快捷键表本身生成的帮助文本 —— 不存在「帮助页忘了更新」这种事。
    #[must_use]
    pub fn shortcuts_help(&self) -> String {
        let mut rows: Vec<(&'static str, &'static str)> = self
            .by_accel
            .values()
            .filter(|(_, _, hint)| !hint.is_empty())
            .map(|(_, label, hint)| (*hint, *label))
            .collect();
        rows.sort();
        rows.into_iter()
            .map(|(hint, label)| format!("{hint}  {label}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 把菜单事件翻译成命令。
    #[must_use]
    pub fn action_for(&self, event: &MenuEvent) -> Option<Action> {
        self.by_id.get(&event.id).copied()
    }

    /// 一次取走所有待处理菜单事件（在每帧 tick 里调）。
    pub fn drain_events(&self) -> Vec<Action> {
        let receiver = MenuEvent::receiver();
        let mut actions = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            if let Some(action) = self.action_for(&event) {
                actions.push(action);
            }
        }
        actions
    }

    /// 生成菜单并挂到 GTK 窗口上。
    ///
    /// `container` 传 `window.default_vbox()` —— muda 会在里面插一个 menubar。
    pub fn install<W, C>(&mut self, window: &W, container: Option<&C>) -> muda::Result<()>
    where
        W: gtk::prelude::IsA<gtk::Window> + gtk::prelude::IsA<gtk::Widget>,
        C: gtk::prelude::IsA<gtk::Widget>,
    {
        self.menu.init_for_gtk_window(window, container)
    }
}

/// 构造菜单与两张表。
#[must_use]
pub fn build() -> MenuBar {
    let menu = Menu::new();
    let mut by_accel: HashMap<Accelerator, (Action, &'static str, &'static str)> = HashMap::new();
    let mut by_id: HashMap<MenuId, Action> = HashMap::new();

    for (title, items) in bindings() {
        let submenu = Submenu::new(title, true);
        for binding in items {
            let item = MenuItem::with_id(binding.action.id(), binding.label, true, binding.accel);
            if let Some(accel) = binding.accel {
                by_accel.insert(accel, (binding.action, binding.label, binding.hint));
            }
            by_id.insert(item.id().clone(), binding.action);
            submenu.append(&item).expect("追加菜单项");
        }
        menu.append(&submenu).expect("追加子菜单");
    }

    // Edit 菜单用 GTK 预定义项：复制/粘贴/全选交给聚焦的控件，不跨进程复制文本。
    let edit = Submenu::new("编辑", true);
    edit.append(&PredefinedMenuItem::copy(None)).ok();
    edit.append(&PredefinedMenuItem::paste(None)).ok();
    edit.append(&PredefinedMenuItem::select_all(None)).ok();
    menu.insert(&edit, 1).ok();

    MenuBar {
        menu,
        by_accel,
        by_id,
    }
}
