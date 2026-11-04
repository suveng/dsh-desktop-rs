//! dsh-desktop —— DeepSeek Harness 的 Linux 桌面壳。
//!
//! 壳只做四件事（`docs/architecture.md` D1）：拉起宿主进程、开一个 webview 加载它、
//! 提供原生窗口能力、在它挂掉时给出可操作的诊断。UI 与协议都归上游。

mod clipboard;
mod desktop;
mod embed;
mod hostscan;
mod menu;
mod onboard;
mod update;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tao::dpi::{LogicalPosition, LogicalSize};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::window::WindowBuilder;
use wry::http::Request;
use wry::{NewWindowResponse, WebViewBuilder};

use dsh_platform::instance::{self, Claim, Command, Instance};
use dsh_platform::{WindowState, load_window_state, save_window_state, state_dir};
use dsh_setup::envcheck::{self, Candidate, Source};
use dsh_setup::node;
use dsh_setup::settings::{self as setup_settings, Settings};
use dsh_setup::version::Version;
use dsh_supervisor::{Event as HostEvent, Host, LaunchSpec};

const APP_TITLE: &str = "DeepSeek Harness";
const SKELETON: &str = include_str!("../assets/skeleton.html");
const ONBOARDING: &str = include_str!("../assets/onboarding.html");
const SHIM: &str = include_str!("../assets/shim.js");

/// `DSH_DESKTOP_DEBUG_DOM=1` 时注入：点完 700ms 把浮层 DOM 的样式/文本打回壳里。
/// 浮层入场动画在「窗口从未获得焦点」的环境（比如没有窗口管理器）里不会推进，
/// 于是子元素永远停在 keyframes 的 from 帧（opacity: 0）—— 布局、尺寸全对但字看不见。
/// 这里把菜单的 enter/exit 动画关掉，element 渲染即不透明。
const MENU_ANIM_FIX: &str = r#"
(function () {
  var add = function () {
    if (document.getElementById('dsh-desktop-no-menu-anim')) return;
    var s = document.createElement('style');
    s.id = 'dsh-desktop-no-menu-anim';
    s.textContent = '[data-menu-phase] > *{animation:none !important}'
      + '.backing[data-menu-phase]{animation:none !important}';
    (document.head || document.documentElement).appendChild(s);
  };
  if (document.head) add(); else document.addEventListener('DOMContentLoaded', add, { once: true });
})();
"#;

const DEBUG_DOM: &str = r#"
(function () {
  document.addEventListener('click', function (ev) {
    var t = ev.target, label = '';
    if (t) {
      var n = t;
      for (var i = 0; i < 4 && n; i++) {
        var a = n.getAttribute && n.getAttribute('aria-label');
        if (a) { label = a; break; }
        n = n.parentElement;
      }
      if (!label) label = (t.tagName || '') + '.' + ((t.className || '').toString().slice(0, 24));
    }
    var added = [], changed = [];
    var mo = new MutationObserver(function (recs) {
      recs.forEach(function (r) {
        if (r.type === 'childList') {
          Array.prototype.forEach.call(r.addedNodes, function (n) {
            if (n.nodeType !== 1) return;
            var b = n.getBoundingClientRect();
            added.push({ tag: n.tagName, cls: (n.className || '').toString().slice(0, 30),
              text: (n.innerText || '').trim().slice(0, 44),
              rect: [Math.round(b.left), Math.round(b.top), Math.round(b.width), Math.round(b.height)] });
          });
        } else if (r.type === 'attributes') {
          var e = r.target;
          if (e.nodeType !== 1) return;
          var b2 = e.getBoundingClientRect(), cs = getComputedStyle(e);
          changed.push({ cls: (e.className || '').toString().slice(0, 30), attr: r.attributeName,
            text: (e.innerText || '').trim().slice(0, 44),
            rect: [Math.round(b2.left), Math.round(b2.top), Math.round(b2.width), Math.round(b2.height)],
            disp: cs.display, op: cs.opacity, vis: cs.visibility });
        }
      });
    });
    mo.observe(document.body, { childList: true, subtree: true, attributes: true,
      attributeFilter: ['class', 'style', 'hidden', 'data-state', 'aria-expanded'] });
    setTimeout(function () {
      mo.disconnect();
      var pops = document.querySelectorAll('[popover]').length;
      var open = 0;
      document.querySelectorAll('[popover]').forEach(function (e) { try { if (e.matches(':popover-open')) open++; } catch (_) {} });
      window.ipc.postMessage(JSON.stringify({ kind: 'debug-dom', id: 901,
        data: { target: label, addedCount: added.length, added: added.slice(0, 8),
          changedCount: changed.length, changed: changed.slice(0, 8),
          popovers: pops, popoversOpen: open,
          bodyText: (document.body.innerText || '').replace(/\s+/g, ' ').slice(0, 50) } }));
    }, 3000);
  }, true);
})();
"#;

/// 轮询宿主的间隔。10 Hz 足够让「就绪 → 加载页面」看起来是即时的，又不会空转。
const TICK: Duration = Duration::from_millis(100);

/// 最多保留多少行宿主日志用于诊断。
const LOG_TAIL: usize = 200;

/// 连续崩溃几次之后不再自动重启（M2 契约：重启 2 次后进失败页）。
const MAX_AUTO_RESTARTS: u32 = 2;

/// SIGTERM / SIGINT / SIGHUP 置位。信号处理器里只允许写这个原子量。
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_signal(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// 桌面环境注销、systemd 停止、`kill` 都走这条路。
///
/// 没有它的话，壳被 SIGTERM 打断时 `Drop` 不会跑，宿主就会变成孤儿 ——
/// 这一点在本机（无窗口管理器）实测过。
#[cfg(unix)]
fn install_signal_handlers() {
    // SAFETY: 处理器只做一次原子写入；SA_RESTART 避免打断 GTK 的 syscall。
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_signal as *const () as usize;
        action.sa_flags = libc::SA_RESTART;
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaction(sig, &action, std::ptr::null_mut());
        }
    }
}

/// 把 stderr 接到状态目录里的日志文件。
///
/// 从启动台 / 访达启动的 GUI 进程没有终端，`eprintln!` 全泺进虚空；unified log 也抳不到
/// （`log show --predicate 'process == "dsh-desktop"'` 里一行都没有）。出了问题就只能猜。
/// 这里把 fd 2 直接指向 `state_dir()/dsh-desktop.log`，所有 `eprintln!` 就自动落盘，
/// 而且「打开日志目录」按钮正好打开的就是这个目录。
#[cfg(unix)]
fn redirect_stderr_to_log() -> Option<std::path::PathBuf> {
    use std::os::fd::AsRawFd;

    let dir = dsh_platform::state_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("dsh-desktop.log");
    // 超过 2 MiB 就轮转一份，避免长期开机启动把盘写满。
    if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > 2 * 1024 * 1024) {
        let _ = std::fs::rename(&path, dir.join("dsh-desktop.log.1"));
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    // SAFETY: 启动阶段、还没创建 webview 和子进程；`dup2` 之后 `file` 可以关，fd 2 独立有效。
    unsafe {
        libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO);
    }
    Some(path)
}

/// `MM-DD HH:MM:SS`，给日志做会话分隔用。
#[cfg(unix)]
fn stamp() -> String {
    // SAFETY: `time`/`localtime_r` 都只写我们传进去的 `tm`。
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        format!(
            "{:02}-{:02} {:02}:{:02}:{:02}",
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

fn main() {
    // webview 隔离会把 XDG_DATA_HOME 指到 tmpfs，先把「装 node / dsh 的目录」钉死。
    dsh_platform::pin_paths();
    // 早于任何其它输出去接日志文件，否则第一批诊断行会丢。
    #[cfg(unix)]
    if std::env::var_os("DSH_DESKTOP_NO_LOG_FILE").is_none()
        && let Some(path) = redirect_stderr_to_log()
    {
        eprintln!("dsh-desktop: ==== 会话 {} ====", stamp());
        eprintln!("dsh-desktop: 日志文件 {}", path.display());
    }
    // WebKitGTK 的 DMA-BUF 渲染器在部分驱动上会让 backdrop-filter 浮层的**内容**不绘制：
    // 只看到模糊底、看不到菜单文字（dsh 的菜单表面用 backdrop-filter）。
    // 壳默认关掉它；用户显式设置过就尊重用户。
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        // SAFETY: 创建 webview 之前、单线程启动阶段设置。
        unsafe {
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
    }
    // 从 Dock / Finder 启动时 PATH 只有 /usr/bin:/bin，nvm 装的 node / npm 不在里面：
    // 环境探测会误报「node 未安装」。把登录 shell 的 PATH 回写进进程，下游（检测、
    // npm、安装子进程）就都看得到。
    if std::env::var_os(dsh_setup::shellenv::DISABLE_VAR).is_none() {
        let home = dsh_platform::home_dir();
        let path = dsh_setup::shellenv::detected_path(&home);
        if !path.is_empty() {
            // SAFETY: 单线程启动阶段，还没有任何子进程 / webview 在读环境。
            unsafe {
                std::env::set_var("PATH", path);
            }
        }
    }
    let cli = match Cli::parse(std::env::args().skip(1)) {
        Ok(cli) => cli,
        Err(err) => {
            eprintln!("dsh-desktop: {err}");
            eprintln!("{}", Cli::USAGE);
            std::process::exit(2);
        }
    };

    if cli.help {
        println!("{}", Cli::USAGE);
        return;
    }
    if cli.version {
        println!("dsh-desktop {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    if cli.check_update {
        let status = update::check(cli.update_feed.as_deref(), env!("CARGO_PKG_VERSION"));
        println!("UPDATE {}", status.to_json());
        return;
    }

    if cli.onboard_selftest {
        std::process::exit(run_onboard_selftest(&cli));
    }

    let code = if cli.probe {
        run_probe(&cli)
    } else {
        run_window(cli)
    };
    std::process::exit(code);
}

// ---------------------------------------------------------------- 命令行

#[derive(Clone)]
struct Cli {
    host_program: Option<PathBuf>,
    /// `--host-cmd "pnpm dsh"`：第一个词是可执行文件，其余是插在标准参数之前的参数。
    host_cmd: Option<Vec<String>>,
    host_args: Vec<String>,
    host_cwd: Option<PathBuf>,
    /// `dsh://…` 深链：由创建者通过 xdg-open 传进来，转发给主实例。
    open_url: Option<String>,
    /// 无窗口模式：只启动宿主、拿到就绪行、打印 origin、退出。CI 与验收脚本用。
    probe: bool,
    /// 有窗口自检：加载真实前端后检查 shim 是否生效，打印 JSON 后退出。M2 冒烟测试用。
    selftest: bool,
    /// 自检用：假装拖入了这些路径，验证 `__DSH_HOST_PATHS__` 的真实路径回填。
    simulate_drop: Vec<String>,
    /// 更新源（JSON：`{"version","url"}`）。缺省时不做任何网络请求。
    update_feed: Option<String>,
    /// 有窗口：在壳内发一条消息（M1「能正常对话」）。
    selftest_chat: bool,
    /// 有窗口：点开工作区「…」菜单并 dump 浮层内容（排查空白浮层）。
    selftest_menu: bool,
    /// 无窗口：只查一次更新并打印 JSON。
    check_update: bool,
    /// 无窗口：跑一遍安装引导（装 node / 装 dsh / 复检），打印日志与结果。
    onboard_selftest: bool,
    version: bool,
    help: bool,
}

impl Cli {
    const USAGE: &'static str = "\
用法: dsh-desktop [选项]

  --host-program <路径>   直接指定宿主可执行文件（默认在 PATH 里找 `dsh`，找不到则回退 `pnpm dsh`）
  --host-cmd <命令>       指定完整启动前缀，如 --host-cmd \"pnpm dsh\"（优先于 --host-program）
                          （都不给时：先找 PATH 里的 dsh，再找 <cwd>/apps/cli/lib/bin.js，最后 pnpm dsh）
  --host-arg <参数>       追加一个宿主参数（可重复），排在标准参数之后
  --host-cwd <目录>       宿主的工作目录
  --probe                 无窗口：启动宿主并打印就绪 origin 后退出（不打印 token）
  --selftest              有窗口：加载真实前端并检查桌面 shim，打印 JSON 后退出
  --simulate-drop <path>  自检用：假装拖入一个路径（可重复），验证真实路径回填
  --update-feed <url>     更新源（JSON：{version,url}）；默认读 DSH_DESKTOP_UPDATE_FEED
  --check-update          无窗口：查一次更新并打印 JSON
  --onboard-selftest      无窗口：跑一遍安装引导（装 node / 装 dsh / 复检）并打印日志
  --selftest-chat         有窗口：在壳内发一条消息（配合会话库核对）
  -V, --version           打印版本
  -h, --help              打印本帮助

环境变量:
  DSH_DESKTOP_HOST        等价于 --host-program";

    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut cli = Self {
            host_program: std::env::var_os("DSH_DESKTOP_HOST").map(PathBuf::from),
            host_cmd: None,
            host_args: Vec::new(),
            host_cwd: None,
            open_url: None,
            probe: false,
            selftest: false,
            simulate_drop: Vec::new(),
            update_feed: std::env::var("DSH_DESKTOP_UPDATE_FEED")
                .ok()
                .filter(|v| !v.is_empty()),
            selftest_chat: false,
            onboard_selftest: false,
            selftest_menu: false,
            check_update: false,
            version: false,
            help: false,
        };

        let mut it = args.peekable();
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--host-program" => {
                    cli.host_program = Some(PathBuf::from(value(&mut it, "--host-program")?));
                }
                "--host-cmd" => {
                    let parts: Vec<String> = value(&mut it, "--host-cmd")?
                        .split_whitespace()
                        .map(str::to_owned)
                        .collect();
                    if parts.is_empty() {
                        return Err("--host-cmd 不能为空".to_owned());
                    }
                    cli.host_cmd = Some(parts);
                }
                "--host-arg" => cli.host_args.push(value(&mut it, "--host-arg")?),
                "--host-cwd" => cli.host_cwd = Some(PathBuf::from(value(&mut it, "--host-cwd")?)),
                "--probe" => cli.probe = true,
                "--selftest" => cli.selftest = true,
                "--simulate-drop" => cli.simulate_drop.push(value(&mut it, "--simulate-drop")?),
                "--update-feed" => cli.update_feed = Some(value(&mut it, "--update-feed")?),
                "--selftest-chat" => cli.selftest_chat = true,
                "--onboard-selftest" => cli.onboard_selftest = true,
                "--selftest-menu" => {
                    cli.selftest = true;
                    cli.selftest_menu = true;
                }
                "--check-update" => cli.check_update = true,
                "-V" | "--version" => cli.version = true,
                "-h" | "--help" => cli.help = true,
                other if other.starts_with("dsh://") => cli.open_url = Some(other.to_owned()),
                other => return Err(format!("未知参数：{other}")),
            }
        }
        Ok(cli)
    }
}

fn value(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    it.next().ok_or_else(|| format!("{flag} 需要一个值"))
}

/// `--profile <名字>` + 监听参数（`startup.ts:64-66`）。
fn standard_args(profile: &str) -> Vec<String> {
    vec![
        "--profile".to_owned(),
        profile.to_owned(),
        "--host".to_owned(),
        "127.0.0.1".to_owned(),
        "--port".to_owned(),
        "0".to_owned(),
        "--no-open".to_owned(),
    ]
}

/// 宿主候选，按优先级：开发树 → 用户目录安装 → 壳自己装的 node 前缀 → PATH → pnpm 兜底。
fn host_candidates(cli: &Cli) -> Vec<Candidate> {
    let mut out = Vec::new();
    // 源码工作区里 `dsh` 全局装的往往是旧版（0.1.x）。工作区编译产物既新又快
    // （就绪 785ms vs `pnpm dsh` 1885ms），所以它优先于 PATH 里的同名命令。
    if let Some(entry) = compiled_harness_entry(cli) {
        out.push(
            Candidate::new("node", Source::Workspace).with_args([entry.display().to_string()]),
        );
    }
    let paths = onboard::Paths::from_platform();
    out.push(Candidate::new(paths.user_dsh(), Source::UserPrefix));
    // 「优先全局安装」在全新机器上会落到壳自己装的 node 前缀里——那条路径不在 PATH 上，
    // 不扫就只能回退到 `pnpm dsh`，然后把装好的环境误判成失败。
    out.extend(onboard::node_prefix_candidates(&paths));
    if let Some(path) = which("dsh") {
        out.push(Candidate::new(path, Source::Path));
    }
    out.push(Candidate::new("pnpm", Source::Pnpm));
    out
}

/// 宿主命令：可执行文件 + 插在标准参数之前的参数。
fn host_command(cli: &Cli) -> (PathBuf, Vec<String>) {
    if let Some(parts) = &cli.host_cmd {
        return (PathBuf::from(&parts[0]), parts[1..].to_vec());
    }
    if let Some(path) = &cli.host_program {
        return (path.clone(), Vec::new());
    }
    let extra = [onboard::Paths::from_platform().user_prefix().join("bin")];
    match envcheck::detect_dsh(&host_candidates(cli), &extra) {
        Some(dsh) => (dsh.program, dsh.leading_args),
        // 一个候选都不在：保持老行为，让失败信息里出现 `pnpm dsh`。
        None => (PathBuf::from("pnpm"), vec!["dsh".to_owned()]),
    }
}

/// 支持区间的下界（`architecture.md` D7）。和引导页共用一份定义，免得两边说法不一致。
const MIN_HOST_VERSION: &str = dsh_setup::settings::MIN_HOST_VERSION;

/// 跑一次 `--version`（带 5 秒超时），拿到宿主版本。
fn probe_host_version(cli: &Cli) -> Option<String> {
    let (program, prefix) = host_command(cli);
    let mut cmd = std::process::Command::new(&program);
    cmd.args(&prefix)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    if let Some(cwd) = &cli.host_cwd {
        cmd.current_dir(cwd);
    }

    let mut child = cmd.spawn().ok()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                return None;
            }
        }
    }

    let mut out = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        use std::io::Read as _;
        stdout.read_to_string(&mut out).ok()?;
    }
    parse_version(&out)
}

fn parse_version(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .map(|token| token.trim_start_matches('v'))
        .find(|token| {
            token.contains('.') && token.chars().next().is_some_and(|c| c.is_ascii_digit())
        })
        .map(str::to_owned)
}

/// 不在支持区间时给一句能贴到 issue 里的话；在区间内返回 `None`。
fn version_note(version: &str) -> Option<String> {
    let supported = Version::parse(version)
        .is_some_and(|parsed| parsed.in_range(&envcheck::min_host(), &envcheck::max_host()));
    if supported {
        return None;
    }
    Some(format!(
        "dsh {version} 不在支持区间（{MIN_HOST_VERSION} ≤ v < {}）",
        dsh_setup::settings::MAX_HOST_VERSION
    ))
}

/// 把 CLI + 设置变成一份启动说明。
fn launch_spec(cli: &Cli, settings: &Settings) -> LaunchSpec {
    let (program, prefix) = host_command(cli);

    let mut spec = LaunchSpec::new(program)
        .args(prefix)
        .args(standard_args(settings.target_profile()));
    if !cli.host_args.is_empty() {
        spec = spec.args(cli.host_args.clone());
    }
    if let Some(cwd) = &cli.host_cwd {
        spec = spec.cwd(cwd);
    }
    // 宿主拿到的 PATH 要能同时看见：用户自己配的目录、引导装出来的 node / dsh、
    // 以及登录 shell 里的 nvm / homebrew —— 否则 `#!/usr/bin/env node` 的 shim
    // 找不到解释器，插件管理里的 `pnpm view` 直接 ENOENT。
    let paths = onboard::Paths::from_platform();
    let home = dsh_platform::home_dir();
    let mut bins: Vec<PathBuf> = settings.extra_path_dirs(&home);
    if !settings.node_version.is_empty() {
        let dir = node::bin_dir(&paths.data, &settings.node_version);
        if dir.is_dir() {
            bins.push(dir);
        }
    }
    let user_bin = paths.user_prefix().join("bin");
    if user_bin.is_dir() {
        bins.push(user_bin);
    }
    let base = dsh_setup::shellenv::detected_path(&home);
    spec = spec.env(
        "PATH",
        dsh_setup::shellenv::compose_path(&bins, Some(base.as_os_str())).to_string_lossy(),
    );
    for (key, value) in settings.host_env() {
        spec = spec.env(key, value);
    }
    spec
}

/// 找到 deepseek-harness 的编译产物入口：`apps/cli/lib/bin.js`。
///
/// 它比 `pnpm dsh` 快一倍以上（785ms vs 1885ms 就绪），直接决定崩溃恢复能不能进 2 秒。
fn compiled_harness_entry(cli: &Cli) -> Option<PathBuf> {
    let base = cli
        .host_cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())?;
    let entry = base.join("apps/cli/lib/bin.js");
    entry.is_file().then_some(entry)
}

fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    let result = {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };
    #[cfg(not(unix))]
    let result = path.is_file();
    result
}

// ---------------------------------------------------------------- 无窗口探针

fn run_probe(cli: &Cli) -> i32 {
    let settings = setup_settings::load(&state_dir());
    let spec = launch_spec(cli, &settings);
    eprintln!("dsh-desktop: 启动 {}", spec.command_line());
    let mut host = match Host::spawn(spec) {
        Ok(host) => host,
        Err(err) => {
            eprintln!("dsh-desktop: 无法启动宿主：{err}");
            return 1;
        }
    };

    let deadline = Instant::now() + Duration::from_secs(45);
    while Instant::now() < deadline {
        for event in host.poll() {
            match event {
                HostEvent::Ready(url) => {
                    // 只打印 origin —— token 属于进程内秘密。
                    println!("ready {}", url.origin());
                    host.stop();
                    return 0;
                }
                HostEvent::Failed(message) => {
                    eprintln!("dsh-desktop: {message}");
                    host.stop();
                    return 1;
                }
                HostEvent::Exited { code } => {
                    eprintln!("dsh-desktop: 宿主在就绪前退出（code={code:?}）");
                    return 1;
                }
                HostEvent::Log { line, .. } => eprintln!("  {line}"),
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    eprintln!("dsh-desktop: 等待就绪超时");
    host.stop();
    1
}

/// 无界面跑一遍安装引导：「自动装 node → 装 dsh → 复检」走的是 GUI 同一套 `Onboard`，
/// 只是不渲染。全新机器验收 / CI 用（`--onboard-selftest`），退出码 0 表示环境就绪。
fn run_onboard_selftest(cli: &Cli) -> i32 {
    let candidates = host_candidates(cli);
    let settings = setup_settings::load(&state_dir());
    let mut onboard = onboard::Onboard::new(onboard::Paths::from_platform(), settings, &candidates);
    onboard.open(&candidates, None);
    println!("ONBOARD ENV {}", onboard.view()["env"]);
    println!("ONBOARD HOSTS {}", onboard.view()["hosts"]);
    if !onboard.env_ready() {
        onboard.handle(
            &serde_json::json!({"kind": "onboard", "cmd": "install"}),
            &candidates,
        );
    }
    let mut printed = 0usize;
    let mut started = false;
    let deadline = Instant::now() + Duration::from_secs(30 * 60);
    while onboard.busy() && Instant::now() < deadline {
        if onboard.tick(&candidates) == onboard::Effect::StartHost {
            started = true;
        }
        if onboard.take_dirty() {
            let _ = setup_settings::save(&state_dir(), &onboard.settings);
        }
        let view = onboard.view();
        if let Some(lines) = view["log"].as_array() {
            for line in lines.iter().skip(printed) {
                println!("  {}", line["text"].as_str().unwrap_or_default());
            }
            printed = lines.len();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let view = onboard.view();
    let ready = onboard.env_ready();
    println!(
        "ONBOARD RESULT {}",
        serde_json::json!({
            "screen": view["screen"],
            "ready": ready,
            "node": view["env"]["node"],
            "dsh": view["env"]["dsh"],
            "start_host": started,
            "error": view["error"],
        })
    );
    i32::from(!ready)
}

// ---------------------------------------------------------------- 窗口

fn run_window(cli: Cli) -> i32 {
    install_signal_handlers();

    // D5：单实例先于一切。第二个进程只负责把请求转过去，然后退出。
    let instance = match Instance::claim() {
        Ok(Claim::Primary(instance)) => {
            // 拿到锁才清：此时上一轮壳已经不在了，留下的宿主一定是孤儿。
            reap_leftover_host();
            instance
        }
        Ok(Claim::Secondary) => {
            let command = cli
                .open_url
                .clone()
                .map_or(Command::Focus, Command::OpenUrl);
            match instance::send(&command) {
                Ok(()) => println!("dsh-desktop: 已有实例在运行，请求已转发"),
                Err(err) => eprintln!("dsh-desktop: 转发到已有实例失败：{err}"),
            }
            return 0;
        }
        Err(err) => {
            eprintln!("dsh-desktop: 单实例检查失败，继续以独立实例启动：{err}");
            // 拿不到锁不该阻止用户用软件；退化成多实例，但要让日志说清楚。
            return run_window_inner(cli, None);
        }
    };

    run_window_inner(cli, Some(instance))
}

fn webview_home() -> std::path::PathBuf {
    dsh_platform::runtime_dir().join("webview")
}

/// 上一个壳留下的宿主进程记录。
fn host_pid_file() -> std::path::PathBuf {
    dsh_platform::runtime_dir().join("host.pid")
}

/// 杀掉上一个壳遗留的宿主。
///
/// 壳被 ⌘Q / SIGKILL / 崩溃带走时（macOS 没有 Linux 的 `PR_SET_PDEATHSIG`），宿主会变成
/// 孤儿进程继续占着端口和实例锁，下次启动就是“双开”。拿到单实例锁时不可能还有别的壳
/// 活着，所以这里可以直接清掉。
#[cfg(unix)]
fn reap_leftover_host() {
    let path = host_pid_file();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(pid) = text.trim().parse::<i32>() else {
        let _ = std::fs::remove_file(&path);
        return;
    };
    if pid <= 1 || pid == std::process::id() as i32 {
        let _ = std::fs::remove_file(&path);
        return;
    }
    // `Host::spawn` 用 `setpgid` 把宿主放进独立进程组，组号就是 pid。
    // SAFETY: `kill` 只传信号，不解引用任何指针。
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    if alive {
        eprintln!("dsh-desktop: 清理上次退出遗留的宿主进程组 {pid}");
        unsafe { libc::kill(-pid, libc::SIGTERM) };
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            if unsafe { libc::kill(pid, 0) } != 0 {
                break;
            }
        }
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
    let _ = std::fs::remove_file(&path);
}

#[cfg(not(unix))]
fn reap_leftover_host() {}

/// R4：带 token 的 URL 不能**落盘**。
///
/// WebKit 会把子资源连同带 `?token=` 的 Referer 写进磁盘缓存，日志脱敏救不了它。
/// 做法是把 XDG 缓存/数据目录指到 `$XDG_RUNTIME_DIR`（tmpfs），并在启动与退出时各清一次。
fn isolate_webview_storage() -> std::path::PathBuf {
    clear_stale_auth_cookies();
    let home = webview_home();
    let _ = std::fs::create_dir_all(&home);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700));
    }

    // 上一版把缓存写在了持久目录里；这里把它们清掉（只删我们自己应用名下的目录）。
    if let Some(base) = std::env::var_os("HOME").map(std::path::PathBuf::from) {
        for legacy in [
            base.join(".cache/dsh-desktop"),
            base.join(".local/share/dsh-desktop"),
        ] {
            if legacy.exists() {
                let _ = std::fs::remove_dir_all(&legacy);
                eprintln!("dsh-desktop: 已清理历史 WebKit 缓存 {}", legacy.display());
            }
        }
    }

    let _ = std::fs::create_dir_all(home.join("cache"));
    let _ = std::fs::create_dir_all(home.join("data"));
    // SAFETY: 这段跑在创建 webview 之前的主线程上，之后没有别的线程读这两个变量。
    unsafe {
        std::env::set_var("XDG_CACHE_HOME", home.join("cache"));
        std::env::set_var("XDG_DATA_HOME", home.join("data"));
    }
    home
}

/// 应用自身的 bundle id（与 `packaging/macos/bundle.sh` 的 `CFBundleIdentifier` 一致）。
const BUNDLE_ID: &str = "dev.suveng.dsh-desktop";

/// macOS 的 WKWebView 不受 XDG 隔离：cookie 落在
/// `~/Library/HTTPStorages/<bundle-id>.binarycookies`。
///
/// 而 dsh 每次握手都会 `set-cookie` 一个**随机命名**的 `dsh-auth-<32>`（Max-Age 30 天），
/// 旧名字永远不会被覆盖。攒到 70 多个以后 `Cookie:` 请求头超过 node 的 16KB 上限，
/// 服务端只回 `431 Request Header Fields Too Large`（空 body）→ 页面白屏（0.1.6 的事故）。
/// 壳启动时（创建 webview 之前）删掉这份 cookie 库：本次会话会重新种一个，够用。
#[cfg(target_os = "macos")]
fn clear_stale_auth_cookies() {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return;
    };
    let dir = home.join("Library/HTTPStorages");
    // 第二个是 0.1.5 之前改名前的老 bundle id，留着同样是坑。
    for name in [
        format!("{BUNDLE_ID}.binarycookies"),
        "dsh-desktop.binarycookies".to_owned(),
    ] {
        let path = dir.join(name);
        if path.exists() && std::fs::remove_file(&path).is_ok() {
            eprintln!(
                "dsh-desktop: 已清理历史 WebKit cookie 库 {}",
                path.display()
            );
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn clear_stale_auth_cookies() {}

fn wipe_webview_storage() {
    let _ = std::fs::remove_dir_all(webview_home());
}

#[cfg(not(unix))]
fn install_signal_handlers() {}

fn run_window_inner(cli: Cli, instance: Option<Instance>) -> i32 {
    let settings = setup_settings::load(&state_dir());
    let spec = launch_spec(&cli, &settings);
    // 显式指定宿主 = 开发者 / CI 路径，环境对不对由他们负责，不弹引导页。
    let explicit_host = cli.host_cmd.is_some() || cli.host_program.is_some();
    // `npm prefix -g` 下的 dsh：只在启动时问一次 npm（几百毫秒），
    // 但每次重探测都带上它——「优先全局安装」装成功就是靠这条认出来的。
    let npm_global = onboard::npm_global_dsh(Path::new("npm"), &settings.install_env())
        .map(|program| Candidate::new(program, envcheck::Source::NpmGlobal));

    // R4：先把 webview 的落盘目录搬到 tmpfs，再谈加载。
    isolate_webview_storage();

    // D7：探测 + 报警，不硬拦。
    let version_note = probe_host_version(&cli).as_deref().and_then(version_note);
    if let Some(note) = &version_note {
        eprintln!("dsh-desktop: {note}");
    }
    let saved = load_window_state().unwrap_or_default();

    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title(format!("{APP_TITLE} — 正在启动…"))
        .with_inner_size(LogicalSize::new(
            f64::from(saved.width),
            f64::from(saved.height),
        ))
        .with_position(LogicalPosition::new(f64::from(saved.x), f64::from(saved.y)))
        .with_min_inner_size(LogicalSize::new(900.0, 600.0))
        .build(&event_loop)
        .expect("创建窗口失败");

    if saved.maximized {
        window.set_maximized(true);
    }

    let mut menubar = menu::build();
    if let Err(err) = menubar.install(&window) {
        eprintln!("dsh-desktop: 安装原生菜单失败：{err}");
    }

    let ipc: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let ipc_sink = Arc::clone(&ipc);
    let drops: Arc<desktop::DropPaths> = Arc::new(desktop::DropPaths::new());
    let drops_sink = Arc::clone(&drops);

    let builder = WebViewBuilder::new()
        .with_html(SKELETON)
        .with_initialization_script(
            if std::env::var_os("DSH_DESKTOP_KEEP_MENU_ANIMATION").is_some() {
                ""
            } else {
                MENU_ANIM_FIX
            },
        )
        .with_initialization_script(SHIM)
        .with_initialization_script(
            std::env::var_os("DSH_DESKTOP_DEBUG_DOM")
                .map(|_| DEBUG_DOM)
                .unwrap_or_default(),
        )
        // 返回 false：保留 DOM 默认的 drop 行为（前端要靠它拿到 File），
        // 我们只是顺手记下原生路径，供 `__DSH_HOST_PATHS__.pathFor()` 查。
        .with_drag_drop_handler(move |event| {
            if let wry::DragDropEvent::Drop { paths, .. } = event {
                drops_sink.record(paths);
                eprintln!(
                    "dsh-desktop: 拖入 {} 个路径，可用于 @path 引用",
                    drops_sink.len()
                );
            }
            false
        })
        .with_ipc_handler(move |request: Request<String>| {
            if let Ok(mut queue) = ipc_sink.lock() {
                queue.push(request.body().clone());
            }
        })
        // 站内导航一律放行，外部链接交给系统浏览器（D2：不做内嵌浏览器分区）。
        .with_navigation_handler(|url| {
            let internal = url.starts_with("http://127.0.0.1")
                || url.starts_with("http://localhost")
                || url.starts_with("about:")
                || url.starts_with("data:");
            if !internal {
                open_external(&url);
            }
            internal
        })
        .with_new_window_req_handler(|url, _features| {
            if url.starts_with("http://127.0.0.1") || url.starts_with("http://localhost") {
                return NewWindowResponse::Allow;
            }
            open_external(&url);
            NewWindowResponse::Deny
        });
    let webview = embed::build(builder, &window);

    let mut app = App {
        host: None,
        spec,
        menubar,
        instance,
        commands: None,
        origin: None,
        zoom: 1.0,
        ready_since: None,
        restart_at: None,
        open_url: cli.open_url.clone(),
        pending_js: Arc::new(Mutex::new(Vec::new())),
        selftest_chat: cli.selftest_chat,
        selftest_menu: cli.selftest_menu,
        menu_clicked: false,
        restart_started: None,
        deep_link_ticks: 0,
        simulate_drop: cli.simulate_drop.clone(),
        tray: None,
        hotkeys: None,
        drops: Arc::clone(&drops),
        version_note,
        update_status: update::Status::idle(),
        update_rx: None,
        update_feed: cli.update_feed.clone(),
        selftest: cli.selftest.then(Selftest::default),
        tick_count: 0,
        ready_tick: 0,
        logs: VecDeque::with_capacity(LOG_TAIL),
        ready: false,
        failure: None,
        ipc,
        webview,
        restarts: 0,
        onboard: {
            let mut candidates = host_candidates(&cli);
            if let Some(global) = &npm_global {
                candidates.push(global.clone());
            }
            onboard::Onboard::new(onboard::Paths::from_platform(), settings, &candidates)
        },
        npm_global,
        cli,
        onboard_rev: 0,
        onboard_log: 0,
        onboard_needs_push: false,
        onboard_acked: false,
        onboard_tries: 0,
        onboard_reloads: 0,
        onboarding: false,
    };
    if let Some(instance) = &app.instance {
        app.commands = Some(instance.commands());
    }
    for path in &app.simulate_drop {
        drops.record([path.as_str()]);
    }
    app.tray = desktop::Tray::create();
    app.hotkeys = desktop::HotKeys::register();
    if app.tray.is_some() {
        eprintln!("dsh-desktop: 托盘已启用");
    }
    // 环境不全就不硬启宿主了：直接上引导页（装 node、装 dsh、选 profile）。
    if explicit_host || app.onboard.env_ready() {
        app.start_host(&window);
    } else {
        eprintln!("dsh-desktop: 运行环境不完整，进入安装引导");
        app.open_onboarding(&window, None);
    }

    event_loop.run(move |event, _target, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + TICK);
        // ⌘Q / 程序塰退出 / 系统注销走的是 AppKit 的 terminate:，不会过 `CloseRequested`。
        // 不在这里收尾，宿主就会变成孤儿继续跑，下次启动就成了双开。
        if let Event::LoopDestroyed = event {
            app.persist_window_state(&window);
            app.shutdown();
            *control_flow = ControlFlow::Exit;
            return;
        }
        if let Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } = event
        {
            app.persist_window_state(&window);
            app.shutdown();
            *control_flow = ControlFlow::Exit;
            return;
        }
        if SHUTDOWN.load(Ordering::SeqCst) {
            app.persist_window_state(&window);
            app.shutdown();
            std::process::exit(0);
        }
        // 「点几下只有一下生效」时，第一步要弄清窗口当时是不是 key window：
        // 非 key 窗口的第一次点击只会激活窗口，不会送进页面。
        if let Event::WindowEvent {
            event: WindowEvent::Focused(focused),
            ..
        } = event
        {
            eprintln!("dsh-desktop: 窗口焦点 {focused}");
        }
        app.tick(&window);
    });
}

/// `--selftest` 的状态机。
#[derive(Default)]
struct Selftest {
    stage: u8,
    results: Vec<String>,
}

struct App {
    host: Option<Host>,
    spec: LaunchSpec,
    menubar: menu::MenuBar,
    instance: Option<Instance>,
    commands: Option<std::sync::mpsc::Receiver<Command>>,
    /// 就绪后的 origin，深链导航需要它（不含 token）。
    origin: Option<String>,
    zoom: f64,
    ready_since: Option<Instant>,
    restart_at: Option<Instant>,
    open_url: Option<String>,
    /// host → 页面 的回填脚本，在 tick 里统一 evaluate。
    pending_js: Arc<Mutex<Vec<String>>>,
    /// 重启分段计时的起点（宿主退出被检测到的时刻）。
    restart_started: Option<Instant>,
    /// `--selftest-chat`：在壳内真的发一条消息（验证输入通路 + 流式渲染）。
    selftest_chat: bool,
    /// `--selftest-menu`：点开工作区「…」菜单并 dump 浮层内容。
    selftest_menu: bool,
    /// 菜单自检只注入一次。
    menu_clicked: bool,
    /// 深链落地后等待页面稳定的计数。
    deep_link_ticks: u32,
    /// 自检用：假装拖入的路径。
    simulate_drop: Vec<String>,
    /// 托盘（探测通过才创建）、全局热键、拖拽路径表。
    tray: Option<desktop::Tray>,
    hotkeys: Option<desktop::HotKeys>,
    drops: Arc<desktop::DropPaths>,
    /// 启动时探测到的宿主版本不在支持区间时的说明（D7）。
    version_note: Option<String>,
    /// 更新提示（§7-7）。没有配置更新源时保持 `idle`，不发任何请求。
    update_status: update::Status,
    update_rx: Option<std::sync::mpsc::Receiver<update::Status>>,
    update_feed: Option<String>,
    /// `--selftest` 的阶段与结果。
    selftest: Option<Selftest>,
    /// 自检看的是「加载完成之后又跑了几帧」，用帧数比用时钟更稳。
    tick_count: u32,
    ready_tick: u32,
    logs: VecDeque<String>,
    ready: bool,
    failure: Option<(String, String)>,
    ipc: Arc<Mutex<Vec<String>>>,
    webview: wry::WebView,
    restarts: u32,
    /// 安装引导：环境不全、或启动失败时接管界面。
    onboard: onboard::Onboard,
    /// `npm prefix -g` 下的 dsh（启动时问一次 npm 的结果）。
    npm_global: Option<Candidate>,
    /// 引导页装完东西后要按新设置重新拼启动说明，所以命令行也得留着。
    cli: Cli,
    /// 上一次推给页面的快照版本 / 日志行数，避免每帧重画。
    onboard_rev: u64,
    onboard_log: usize,
    /// 页面刚加载或刚处理完命令：下一帧强制推一次快照。
    onboard_needs_push: bool,
    /// 页面脚本回报过「就位」。冷启动时 WebKit 起进程要几百毫秒，第一帧的快照会打在空白文档上
    /// （`window.__onboard &&` 守卫静默成功），没这个握手页面就会一直空着。
    onboard_acked: bool,
    /// 未握手期间已经补推过几次（等太久就别一直重推）。
    onboard_tries: u8,
    /// 引导页文档重载过几次（脚本没跑起来时自愈用，别无限重试）。
    onboard_reloads: u8,
    /// 当前 webview 里是引导页 / 报错页（而不是宿主页面）。
    onboarding: bool,
}

impl App {
    /// 宿主候选：每次都按当前状态重算。
    ///
    /// 不能只在启动时算一次——刚 `npm i -g` 装好的 dsh 那时还不存在，
    /// 复检用旧名单就会永远卡在「环境不全」。
    fn candidates(&self) -> Vec<Candidate> {
        let mut candidates = host_candidates(&self.cli);
        // `npm prefix -g` 是在启动时问的一次（起 npm 几百毫秒），前缀不会变，
        // 但 dsh 是后来才装进去的——所以这里按“现在是否存在”过滤，不能缓存成 Option<Candidate>。
        if let Some(global) = &self.npm_global
            && global.program.is_file()
            && !candidates.iter().any(|c| c.program == global.program)
        {
            candidates.push(global.clone());
        }
        candidates
    }

    fn title_for(&self, base: &str) -> String {
        match &self.version_note {
            Some(note) => format!("{base} · {note}"),
            None => base.to_owned(),
        }
    }

    fn start_host(&mut self, window: &tao::window::Window) {
        if let Some(mut old) = self.host.take() {
            old.stop();
        }
        // 引导页可能刚装完 node / dsh 或换了 profile：重新拼一份启动说明。
        self.spec = launch_spec(&self.cli, &self.onboard.settings);
        self.onboard.close();
        self.onboard_needs_push = false;
        self.onboard_acked = false;
        self.onboard_tries = 0;
        self.ready = false;
        self.failure = None;
        self.logs.clear();
        self.webview.load_html(SKELETON).ok();
        self.onboarding = false;
        window.set_title(&self.title_for(&format!("{APP_TITLE} — 正在启动…")));
        eprintln!("dsh-desktop: 启动 {}", self.spec.command_line());

        match Host::spawn(self.spec.clone()) {
            Ok(host) => {
                let _ = std::fs::create_dir_all(dsh_platform::runtime_dir());
                let _ = std::fs::write(host_pid_file(), host.pid().to_string());
                self.host = Some(host);
            }
            Err(err) => {
                self.fail(
                    window,
                    "无法启动本地服务",
                    &format!("执行 {} 失败：{err}", self.spec.command_line()),
                );
            }
        }
    }

    fn tick(&mut self, window: &tao::window::Window) {
        self.tick_count = self.tick_count.saturating_add(1);
        self.flush_pending_js();
        self.poll_update();
        self.drive_selftest();
        self.finish_selftest_if_done();
        self.drain_ipc(window);
        self.drive_onboard(window);
        self.drain_menu(window);
        self.drain_commands(window);
        self.drain_desktop(window);
        self.settle_deep_link();
        self.drive_chat_selftest();
        self.drive_menu_selftest();

        if let Some(at) = self.restart_at
            && Instant::now() >= at
        {
            self.restart_at = None;
            self.start_host(window);
        }

        // 熔断复位：稳定跑满一分钟就把「连续崩溃」计数清零，
        // 否则隔几天崩两次就会永久进入失败页。
        if let Some(since) = self.ready_since
            && since.elapsed() >= Duration::from_secs(60)
        {
            self.restarts = 0;
            self.ready_since = None;
        }

        let Some(host) = self.host.as_mut() else {
            return;
        };
        for event in host.poll() {
            match event {
                HostEvent::Ready(url) => {
                    if let Some(started) = self.restart_started.take() {
                        eprintln!(
                            "dsh-desktop: 重启完成，检测到就绪用时 {} ms",
                            started.elapsed().as_millis()
                        );
                    }
                    self.ready = true;
                    self.ready_tick = self.tick_count;
                    self.origin = Some(url.origin().to_owned());
                    self.ready_since = Some(Instant::now());
                    let target = self.navigate_deep_link();
                    self.start_update_check();
                    // 先禁用 replaceState 试验：/1 研究空白浮层是否由它导致
                    // self.strip_token_from_location();
                    window.set_title(&self.title_for(APP_TITLE));
                    if let Err(err) = self
                        .webview
                        .load_url(target.as_deref().unwrap_or(url.as_str()))
                    {
                        self.fail(
                            window,
                            "加载界面失败",
                            &format!("导航到本地服务失败：{err}"),
                        );
                    }
                }
                HostEvent::Log { line, .. } => {
                    if self.logs.len() == LOG_TAIL {
                        self.logs.pop_front();
                    }
                    self.logs.push_back(line);
                }
                HostEvent::Failed(message) => {
                    self.fail(window, "本地服务没有启动起来", &message);
                }
                HostEvent::Exited { code } => {
                    if self.restarts < MAX_AUTO_RESTARTS {
                        self.restarts += 1;
                        self.restart_started = Some(Instant::now());
                        // 第一次崩溃立刻重启（争取 2 秒内恢复）；之后才退避，避免崩溃循环烧 CPU。
                        let delay = if self.restarts == 1 {
                            Duration::ZERO
                        } else {
                            Duration::from_secs(1)
                        };
                        self.restart_at = Some(Instant::now() + delay);
                        self.ready = false;
                        self.ready_since = None;
                        window
                            .set_title(&self.title_for(&format!("{APP_TITLE} — 本地服务重启中…")));
                        self.webview.load_html(SKELETON).ok();
                    } else {
                        self.fail(
                            window,
                            "本地服务反复退出",
                            &format!(
                                "宿主进程退出（code={code:?}），已自动重启 {MAX_AUTO_RESTARTS} 次。\n你可以点「重启本地服务」再试一次，或用 --probe 看宿主自己输出了什么。"
                            ),
                        );
                    }
                }
                HostEvent::Starting { .. } | HostEvent::Stopped => {}
            }
        }
    }

    fn drain_ipc(&mut self, window: &tao::window::Window) {
        let commands: Vec<String> = {
            let mut queue = match self.ipc.lock() {
                Ok(queue) => queue,
                Err(poisoned) => poisoned.into_inner(),
            };
            std::mem::take(&mut *queue)
        };

        for command in commands {
            match command.as_str() {
                "retry" => {
                    self.restarts = self.restarts.saturating_add(1);
                    self.start_host(window);
                }
                "open-logs" => {
                    let dir = state_dir();
                    let _ = std::fs::create_dir_all(&dir);
                    open_external(&dir.display().to_string());
                }
                "quit" => {
                    self.persist_window_state(window);
                    self.shutdown();
                    std::process::exit(0);
                }
                // 页面侧粘贴到「没有文件也没有文本」的剪贴板时，请壳把位图送进来。
                "clipboard-image" => self.deliver_clipboard_image(),
                other if other.starts_with('{') => {
                    // 引导页的请求带 `kind:"onboard"`，别拿去喂给 shim（它只认 `id`）。
                    let routed = serde_json::from_str::<serde_json::Value>(other)
                        .ok()
                        .filter(|request| {
                            request.get("kind").and_then(serde_json::Value::as_str)
                                == Some("onboard")
                        });
                    match routed {
                        Some(request) => {
                            // 引导页脚本跑完会自己报「ready」：这时才相信后续的 evaluate_script 落到了正确的文档上。
                            if request.get("cmd").and_then(serde_json::Value::as_str)
                                == Some("ready")
                            {
                                self.onboard_acked = true;
                                self.onboard_needs_push = true;
                            } else {
                                eprintln!("dsh-desktop: 引导页指令 {request}");
                                let effect = self.onboard.handle(&request, &self.candidates());
                                // 处理过一条命令就推一次新快照：以前这里靠每帧无条件推送，
                                // 结果页面每 100ms 重建一次按钮，mousedown 与 mouseup 落在
                                // 不同节点上，`click` 永远不触发（自定义按钮全部「点不动」）。
                                self.onboard_needs_push = true;
                                self.apply_onboard_effect(window, effect);
                            }
                        }
                        None => self.handle_shim(other),
                    }
                }
                other => eprintln!("dsh-desktop: 未知的界面指令 {other:?}"),
            }
        }
    }

    // 曾经的 `history.replaceState(null,'','/')` 已移除：它会把文档 URL 改成不带 token 的根地址，
    // 之后任何依赖 URL 鉴权的取数都会掉 303（正是「菜单只画框不画字」的早期怀疑对象之一）。
    // token 不落盘由 isolate_webview_storage() + 退出清理保证，不需要再动页面 URL。

    fn start_update_check(&mut self) {
        let Some(feed) = self.update_feed.clone() else {
            return;
        };
        if self.update_rx.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.update_rx = Some(rx);
        let current = env!("CARGO_PKG_VERSION").to_owned();
        std::thread::spawn(move || {
            let status = update::check(Some(&feed), &current);
            let _ = tx.send(status);
        });
    }

    fn poll_update(&mut self) {
        let Some(rx) = self.update_rx.as_ref() else {
            return;
        };
        let Ok(status) = rx.try_recv() else {
            return;
        };
        self.update_rx = None;
        match status.phase {
            "available" => eprintln!(
                "dsh-desktop: 有新版本 {}（当前 {}）",
                status.version.as_deref().unwrap_or("?"),
                env!("CARGO_PKG_VERSION")
            ),
            "error" => eprintln!(
                "dsh-desktop: 检查更新失败：{}",
                status.failure.as_deref().unwrap_or("未知原因")
            ),
            _ => {}
        }
        if status.phase == "available" {
            desktop::notify(
                "有新版本可用",
                &format!(
                    "{} → {}",
                    env!("CARGO_PKG_VERSION"),
                    status.version.as_deref().unwrap_or("?")
                ),
            );
        }
        self.emit_update(&status);
        self.update_status = status;
    }

    /// 把更新状态推给页面里的监听者（shim 的 `updates.subscribe`）。
    fn emit_update(&self, status: &update::Status) {
        if let Ok(mut queue) = self.pending_js.lock() {
            queue.push(format!(
                "window.__dshShimEmitUpdate && window.__dshShimEmitUpdate({});",
                status.to_json()
            ));
        }
    }

    fn check_update_now(&mut self, window: &tao::window::Window) {
        if self.update_feed.is_none() {
            self.show_message(window, "检查更新", "没有配置更新源。\n用 --update-feed <url> 或环境变量 DSH_DESKTOP_UPDATE_FEED 指定。");
            return;
        }
        self.update_rx = None;
        self.start_update_check();
        self.show_message(
            window,
            "检查更新",
            &format!("正在检查…（当前版本 {}）", env!("CARGO_PKG_VERSION")),
        );
    }

    fn show_message(&self, _window: &tao::window::Window, title: &str, body: &str) {
        rfd::MessageDialog::new()
            .set_title(title)
            .set_description(body)
            .set_level(rfd::MessageLevel::Info)
            .show();
    }

    /// 处理来自 `assets/shim.js` 的请求。
    fn handle_shim(&mut self, raw: &str) {
        let Ok(request) = serde_json::from_str::<serde_json::Value>(raw) else {
            eprintln!("dsh-desktop: 无法解析的页面请求");
            return;
        };
        let Some(id) = request.get("id").and_then(serde_json::Value::as_u64) else {
            return;
        };
        let kind = request
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();

        match kind {
            "device-info" => {
                let info = device_info();
                if let Some(state) = self.selftest.as_mut() {
                    // 自检直接在这里记录结果，不再二次 evaluate 读回 —— 少一次时序依赖。
                    state.results.push(format!(
                        "{{\"device\":{}}}",
                        serde_json::Value::String(info.clone())
                    ));
                }
                self.resolve(id, true, &serde_json::Value::String(info));
            }
            "host-path" => {
                let name = request
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let resolved = self.drops.resolve(name);
                if let Some(state) = self.selftest.as_mut() {
                    state.results.push(format!(
                        "{{\"hostPath\":{}}}",
                        serde_json::Value::String(resolved.clone().unwrap_or_default())
                    ));
                }
                self.resolve(
                    id,
                    true,
                    &serde_json::Value::String(resolved.unwrap_or_default()),
                )
            }
            "update-status" => {
                let status: serde_json::Value = serde_json::from_str(&self.update_status.to_json())
                    .unwrap_or(serde_json::Value::Null);
                self.resolve(id, true, &status);
            }
            "update-open" => {
                if let Some(url) = self.update_status.url.clone() {
                    spawn_detached("xdg-open", &[&url]);
                }
                self.resolve(id, true, &serde_json::Value::Null);
            }
            "debug-dom" => {
                eprintln!(
                    "DOM-DEBUG {}",
                    request
                        .get("data")
                        .map(|d| d.to_string())
                        .unwrap_or_else(|| "null".to_owned())
                );
                self.resolve(id, true, &serde_json::Value::Null);
            }
            "probe-menu" => {
                // 页面侧定时采集，Rust 侧只负责收 —— 不依赖壳的帧循环。
                if let Some(data) = request.get("data")
                    && let Some(state) = self.selftest.as_mut()
                {
                    state.results.push(data.to_string());
                }
                self.resolve(id, true, &serde_json::Value::Null);
            }
            "clipboard-image" => {
                self.deliver_clipboard_image();
                self.resolve(id, true, &serde_json::Value::Null);
            }
            "pick-directory" => match pick_directory() {
                Some(path) => self.resolve(id, true, &serde_json::Value::String(path)),
                None => self.resolve(id, false, &serde_json::Value::String("已取消".to_owned())),
            },
            other => {
                eprintln!("dsh-desktop: 页面请求了未实现的能力 {other:?}");
                self.resolve(
                    id,
                    false,
                    &serde_json::Value::String(format!("暂不支持：{other}")),
                );
            }
        }
    }

    fn resolve(&self, id: u64, ok: bool, value: &serde_json::Value) {
        let payload = serde_json::to_string(value).unwrap_or_else(|_| "null".to_owned());
        if let Ok(mut queue) = self.pending_js.lock() {
            queue.push(format!(
                "window.__dshShimResolve && window.__dshShimResolve({id}, {ok}, {payload});"
            ));
        }
    }

    fn flush_pending_js(&mut self) {
        let scripts: Vec<String> = {
            let mut queue = match self.pending_js.lock() {
                Ok(queue) => queue,
                Err(poisoned) => poisoned.into_inner(),
            };
            std::mem::take(&mut *queue)
        };
        for script in scripts {
            if script.contains("__dshShimResolve(") {
                eprintln!("dsh-desktop: shim 回填 {script}");
            }
            if let Some(value) = script.strip_prefix("__SELFTEST__") {
                match self.selftest.as_mut() {
                    Some(state) => state.results.push(value.to_owned()),
                    // 非自检模式（如 --selftest-chat）下，同样的探针输出转成诊断日志。
                    None => eprintln!("dsh-desktop: [diag] {value}"),
                }
                continue;
            }
            if let Err(err) = self.webview.evaluate_script(&script) {
                eprintln!("dsh-desktop: 回填页面调用失败：{err}");
            }
        }
    }

    /// 有窗口自检：三步 —— 静态检查 → 调 deviceInfo → 读回结果。
    fn drive_selftest(&mut self) {
        let Some(state) = self.selftest.as_mut() else {
            return;
        };
        if !self.ready || self.tick_count < 20 {
            return;
        }

        if state.stage == 0 {
            state.stage = 1;
            // 版本区间（D7）与更新状态也一起进自检输出，免得只能靠看标题。
            let version_note = self
                .version_note
                .clone()
                .unwrap_or_else(|| "in-range".to_owned());
            let update_phase = self.update_status.phase.to_owned();
            if let Some(state) = self.selftest.as_mut() {
                state.results.push(format!(
                    "{{\"versionNote\":{},\"updatePhase\":{}}}",
                    serde_json::Value::String(version_note),
                    serde_json::Value::String(update_phase)
                ));
            }
            // 主动调一次 deviceInfo：既验证 IPC 往返，也顺带把结果记进自检输出。
            self.webview
                .evaluate_script("window.dshDesktop.deviceInfo();")
                .ok();
            if let Some(first) = self.simulate_drop.first() {
                let name = std::path::Path::new(first)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let script = format!(
                    "window.__DSH_HOST_PATHS__.pathFor({{name:{}}});",
                    serde_json::Value::String(name)
                );
                self.webview.evaluate_script(&script).ok();
            }
            let tray_on = self.tray.is_some();
            let hotkeys_on = self.hotkeys.is_some();
            let notify_on = desktop::notify("dsh-desktop 自检", "桌面集成自检通知");
            if let Some(state) = self.selftest.as_mut() {
                state.results.push(format!(
                    "{{\"tray\":{tray_on},\"hotkeys\":{hotkeys_on},\"notify\":{notify_on}}}"
                ));
            }
            self.probe_script(
                "JSON.stringify({\
                   marker: document.documentElement.getAttribute('data-dsh-desktop-shim'),\
                   protocolVersion: window.dshDesktop && window.dshDesktop.protocolVersion,\
                   directoryPicker: !!window.__DSH_DIRECTORY_PICKER__,\
                   hostPaths: !!window.__DSH_HOST_PATHS__,\
                   deviceInfoType: typeof (window.dshDesktop || {}).deviceInfo,\
                   pluginError: (document.body.innerText || '').indexOf('Failed to load plugins') >= 0\
                 })",
            );
        }
    }

    fn probe_script(&self, script: &str) {
        let pending = Arc::clone(&self.pending_js);
        self.webview
            .evaluate_script_with_callback(script, move |value| {
                if let Ok(mut queue) = pending.lock() {
                    queue.push(format!("__SELFTEST__{value}"));
                }
            })
            .ok();
    }

    fn finish_selftest_if_done(&mut self) {
        let Some(state) = self.selftest.as_ref() else {
            return;
        };
        // 必须真的等到 deviceInfo 往返的结果（自检的第一条是版本区间，不算数）。
        let has_device = state.results.iter().any(|row| row.contains("\"device\""));
        // deviceInfo 走 IPC 回填，可能早于页面内的 shim 探测；两者都到齐才算完成。
        let has_shim = state
            .results
            .iter()
            .any(|row| row.contains("protocolVersion"));
        if !has_shim {
            return;
        }
        let has_host_path = self.simulate_drop.is_empty()
            || state.results.iter().any(|row| row.contains("\"hostPath\""));
        let menu_done =
            !self.selftest_menu || state.results.iter().any(|row| row.contains("\"blur\""));
        if !has_device || !has_host_path || !menu_done {
            return;
        }
        let output = state.results.join(" ");
        println!("SELFTEST {output}");
        // 自检退出也必须走优雅关闭，否则会留下 4 个孤儿宿主。
        self.shutdown();
        std::process::exit(0);
    }

    /// 托盘点击与全局热键。
    fn drain_desktop(&mut self, window: &tao::window::Window) {
        if let Some(tray) = &self.tray {
            for action in tray.drain() {
                match action {
                    desktop::TRAY_SHOW => {
                        window.set_visible(true);
                        window.set_focus();
                    }
                    desktop::TRAY_ONBOARD => self.open_onboarding(window, None),
                    desktop::TRAY_RELOAD => {
                        self.webview.reload().ok();
                    }
                    desktop::TRAY_QUIT => {
                        self.persist_window_state(window);
                        self.shutdown();
                        std::process::exit(0);
                    }
                    _ => {}
                }
            }
        }
        if self
            .hotkeys
            .as_ref()
            .is_some_and(desktop::HotKeys::triggered)
        {
            window.set_visible(true);
            window.set_focus();
        }
    }

    /// 深链目标没有对应路由时是空页：2 秒后仍没内容就退回根页面。
    fn settle_deep_link(&mut self) {
        if self.deep_link_ticks == 0 {
            return;
        }
        self.deep_link_ticks += 1;
        if self.deep_link_ticks == 20 {
            self.deep_link_ticks = 0;
            self.probe_script(
                "JSON.stringify({ deepLinkBody: (document.body.innerText || '').trim().length })",
            );
        }
    }

    /// 关键点：先把 webview 自己 focus 上，再走 execCommand ——
    /// 之前只 focus 了 DOM 节点，GTK 层没有焦点，输入命令全部被丢弃。
    /// A 层：只验证「输入能进 composer」。渲染层由 B（网络路径）单独覆盖。
    /// 页面是渐进渲染的，所以注入自轮询脚本，等 composer 出现再输入。
    fn drive_chat_selftest(&mut self) {
        if !self.selftest_chat || self.ready_tick == 0 {
            return;
        }
        let since = self.tick_count.saturating_sub(self.ready_tick);
        if since == 10 {
            self.webview.focus().ok();
            self.webview
                .evaluate_script(
                    r#"
                    (function () {
                      var started = Date.now();
                      var iv = setInterval(function () {
                        var ce = document.querySelector('[contenteditable="true"]');
                        if (ce && !window.__dshTyped) {
                          ce.focus();
                          window.__dshExec = document.execCommand('insertText', false, 'CHATOK');
                          window.__dshTyped = true;
                          var b = Array.prototype.slice.call(document.querySelectorAll('button')).filter(function (x) {
                            return /发送消息/i.test((x.getAttribute('aria-label') || x.innerText || '').trim());
                          })[0];
                          if (b) { b.click(); window.__dshClicked = true; }
                          clearInterval(iv);
                        }
                        if (Date.now() - started > 20000) { clearInterval(iv); }
                      }, 300);
                    })();
                    "#,
                )
                .ok();
        }
        if since == 40 || since == 70 {
            self.probe_script(
                "JSON.stringify({ stage: 'chat', typed: !!window.__dshTyped, clicked: !!window.__dshClicked, ws: (window.__dshWs||{}), echo: (document.body.innerText||'').indexOf('CHATOK') >= 0, bodyLen: (document.body.innerText||'').length })",
            );
        }
    }

    /// 点开工作区「…」菜单，再把浮层里的真实内容 dump 出来。
    fn drive_menu_selftest(&mut self) {
        if !self.selftest_menu || self.menu_clicked || self.ready_since.is_none() {
            return;
        }
        self.menu_clicked = true;
        // 采集放页面里做（setTimeout），壳只等 IPC 回来的结果。
        self.webview
            .evaluate_script(
                r#"
                (function () {
                  setTimeout(function () {
                    var b = Array.prototype.slice.call(document.querySelectorAll('button')).filter(function (x) {
                      return /工作区.*的操作/.test((x.getAttribute('aria-label') || '').trim());
                    })[0];
                    window.__dshMenuBtn = !!b;
                    if (b) b.click();
                  }, 3000);
                  setTimeout(function () {
                    var menus = Array.prototype.slice.call(document.querySelectorAll('[role="menu"],[data-radix-popper-content-wrapper],[role="dialog"]'));
                    var data = {
                      btn: window.__dshMenuBtn === true,
                      count: menus.length,
                      text: menus.map(function (m) { return (m.innerText || '').trim().slice(0, 60); }),
                      cls: menus.map(function (m) { return (m.className || '').toString().slice(0, 40); }),
                      blur: menus.map(function (m) { return getComputedStyle(m).backdropFilter; }),
                      rect: menus.map(function (m) { var r = m.getBoundingClientRect(); return Math.round(r.width) + 'x' + Math.round(r.height); })
                    };
                    window.ipc.postMessage(JSON.stringify({ kind: 'probe-menu', id: 900, data: data }));
                  }, 7000);
                })();
                "#,
            )
            .ok();
    }

    fn drain_menu(&mut self, window: &tao::window::Window) {
        for action in self.menubar.drain_events() {
            match action {
                // 引导页里按 Ctrl+R 没东西可“重新加载”，直接当成“回主界面”。
                menu::Action::Reload => {
                    if self.onboarding {
                        self.start_host(window);
                    } else if let Err(err) = self.webview.reload() {
                        eprintln!("dsh-desktop: 重新加载失败：{err}");
                    }
                }
                menu::Action::Main => self.start_host(window),
                menu::Action::Onboard => self.open_onboarding(window, None),
                menu::Action::OpenLogs => self.open_logs(),
                menu::Action::PasteScreenshot => self.deliver_clipboard_image(),
                menu::Action::Quit => {
                    self.persist_window_state(window);
                    self.shutdown();
                    std::process::exit(0);
                }
                menu::Action::ZoomIn => self.set_zoom(self.zoom + 0.1),
                menu::Action::ZoomOut => self.set_zoom(self.zoom - 0.1),
                menu::Action::ZoomReset => self.set_zoom(1.0),
                menu::Action::ToggleFullscreen => {
                    let fullscreen = window.fullscreen();
                    if fullscreen.is_some() {
                        window.set_fullscreen(None);
                    } else {
                        window.set_fullscreen(Some(tao::window::Fullscreen::Borderless(None)));
                    }
                }
                menu::Action::Minimize => window.set_minimized(true),
                menu::Action::About => self.show_about(window),
                menu::Action::CheckUpdate => self.check_update_now(window),
            }
        }
    }

    /// 把剪贴板里的截图当附件送进页面。
    ///
    /// 三段式：`arboard` 读位图 → 编成 PNG → 分块注入一段脚本合成 `drop`。
    /// 走 drop 是因为 `DragEvent` 允许自带 `dataTransfer`，而 `ClipboardEvent` 的
    /// `clipboardData` 由浏览器持有（见 `clipboard.rs` 头部的 WebKit bug）。
    fn deliver_clipboard_image(&mut self) {
        let Some(png) = clipboard::read_png() else {
            eprintln!("dsh-desktop: 剪贴板里没有可粘贴的图片（只有文本或根本是空的）");
            return;
        };
        if clipboard::was_just_delivered(&png) {
            return;
        }
        for script in clipboard::paste_scripts(&png) {
            if let Err(err) = self.webview.evaluate_script(&script) {
                eprintln!("dsh-desktop: 注入剪贴板图片失败：{err}");
                return;
            }
        }
        eprintln!(
            "dsh-desktop: 已把剪贴板图片（{} KiB）交给页面",
            png.len() / 1024
        );
    }

    fn drain_commands(&mut self, window: &tao::window::Window) {
        let mut received = Vec::new();
        if let Some(commands) = &self.commands {
            while let Ok(command) = commands.try_recv() {
                received.push(command);
            }
        }
        for command in received {
            match command {
                Command::Focus => {
                    // 最小化状态下 set_focus 不会把窗口拉回来，先解除最小化。
                    window.set_minimized(false);
                    window.set_visible(true);
                    window.set_focus();
                    #[cfg(target_os = "linux")]
                    {
                        use gtk::prelude::GtkWindowExt as _;
                        use tao::platform::unix::WindowExtUnix as _;
                        window.gtk_window().present();
                    }
                }
                Command::OpenUrl(url) => {
                    self.open_url = Some(url);
                    if self.ready {
                        let _ = self.navigate_deep_link();
                    }
                    self.start_update_check();
                    // 先禁用 replaceState 试验：/1 研究空白浮层是否由它导致
                    // self.strip_token_from_location();
                }
            }
        }
    }

    /// `dsh://x/y?z` → `<origin>/x/y?z`。
    ///
    /// dsh 前端没有对外文档化的路由，所以这条映射是壳自己的约定（见 `architecture.md` §7-15）。
    /// `dsh://x/y?z` → `<origin>/x/y?z`；返回 `None` 表示本次无需导航。
    fn navigate_deep_link(&mut self) -> Option<String> {
        let origin = self.origin.clone()?;
        let raw = self.open_url.take()?;
        let rest = raw
            .strip_prefix("dsh://")
            .unwrap_or(&raw)
            .trim_start_matches('/');
        if rest.is_empty() {
            return None;
        }
        self.deep_link_ticks = 1;
        Some(format!("{origin}/{rest}"))
    }

    fn set_zoom(&mut self, zoom: f64) {
        self.zoom = zoom.clamp(0.5, 2.5);
        if let Err(err) = self.webview.zoom(self.zoom) {
            eprintln!("dsh-desktop: 缩放失败：{err}");
        }
    }

    fn open_logs(&mut self) {
        let dir = state_dir();
        let _ = std::fs::create_dir_all(&dir);
        // macOS 上 xdg-open 不存在，这个按钮会静默没反应；走平台分发。
        open_external(&dir.display().to_string());
    }

    fn show_about(&self, _window: &tao::window::Window) {
        rfd::MessageDialog::new()
            .set_title("dsh-desktop")
            .set_description(format!(
                "版本 {}\n\nDeepSeek Harness 的桌面壳。界面与协议归上游，壳只负责窗口、进程与桌面集成。\n\n快捷键\n{}",
                env!("CARGO_PKG_VERSION"),
                self.menubar.shortcuts_help()
            ))
            .set_level(rfd::MessageLevel::Info)
            .show();
    }

    fn fail(&mut self, window: &tao::window::Window, summary: &str, detail: &str) {
        self.failure = Some((summary.to_owned(), detail.to_owned()));
        // 标题用 ASCII，避免 CI 的 locale 影响 xdotool 匹配；中文留在页面里。
        window.set_title(&format!("{APP_TITLE} — startup failed"));
        desktop::notify(summary, detail);
        // 任何启动失败都进引导页：它既能修环境（装 node / dsh、换 profile），
        // 也把这次失败的诊断（命令 + 宿主日志尾巴）留在日志面板里。
        let command = self.spec.command_line();
        self.onboard
            .note_failure(summary, detail, &command, &self.logs);
        self.open_onboarding(window, Some((summary.to_owned(), detail.to_owned())));
    }

    /// 切到安装引导：停掉宿主，换成引导页。
    fn open_onboarding(&mut self, window: &tao::window::Window, banner: Option<(String, String)>) {
        if let Some(mut host) = self.host.take() {
            host.stop();
        }
        self.ready = false;
        self.restart_at = None;
        let size = window.inner_size().to_logical::<f64>(window.scale_factor());
        eprintln!(
            "dsh-desktop: 引导页窗口 {}x{}（缩放 {:.2}）",
            size.width.round(),
            size.height.round(),
            window.scale_factor()
        );
        self.onboard.open(&self.candidates(), banner);
        window.set_title(&self.title_for(&format!("{APP_TITLE} — 安装引导")));
        self.webview.load_html(ONBOARDING).ok();
        self.onboarding = true;
        self.onboard_needs_push = true;
        self.onboard_acked = false;
        self.onboard_tries = 0;
        self.onboard_reloads = 0;
    }

    /// 每帧推进引导页：跑执行队列、落盘设置、把快照推给页面。
    fn drive_onboard(&mut self, window: &tao::window::Window) {
        if !self.onboard.visible() {
            return;
        }
        let effect = self.onboard.tick(&self.candidates());
        let (pid, url, ready) = match self.host.as_ref() {
            Some(host) => (Some(host.pid()), self.origin.clone(), host.is_ready()),
            None => (None, None, false),
        };
        self.onboard.set_host_status(pid, url, ready);
        if self.onboard.take_dirty()
            && let Err(err) = setup_settings::save(&state_dir(), &self.onboard.settings)
        {
            eprintln!("dsh-desktop: 保存设置失败：{err}");
        }
        self.apply_onboard_effect(window, effect);
        if !self.onboard.visible() {
            return;
        }

        // 页面还没回报「就位」：每帧补推一次（拿到握手就停）。
        const UNACKED_PUSH_LIMIT: u8 = 50;
        if !self.onboard_acked {
            self.onboard_needs_push = true;
            self.onboard_tries = self.onboard_tries.saturating_add(1);
            if self.onboard_tries == UNACKED_PUSH_LIMIT {
                // 自愈：先用重加载把没跑起来的脚本稿活（WebKit 偶尔把 `with_html` 的文档停在白页）。
                const ONBOARD_RELOAD_LIMIT: u8 = 1;
                if self.onboard_reloads < ONBOARD_RELOAD_LIMIT {
                    self.onboard_reloads += 1;
                    self.onboard_tries = 0;
                    eprintln!(
                        "dsh-desktop: 引导页 5 秒内没有回报就位，重载文档（第 {} 次）",
                        self.onboard_reloads
                    );
                    self.webview.load_html(ONBOARDING).ok();
                } else {
                    eprintln!(
                        "dsh-desktop: 引导页重载后仍未回报就位，停止补推（页面脚本可能没跑起来）"
                    );
                    // 报告页面实况：IPC 句柄在不在、快照钩子建没建、按钮在不在。
                    self.probe_script(
                        r#"JSON.stringify({ipc: typeof window.ipc, hook: typeof window.__onboard, ready: document.readyState, body: document.body.innerText.length, script: document.querySelectorAll('script').length, button: (document.querySelector('[data-cmd="probe"]')||{}).tagName || null})"#,
                    );
                    self.probe_script(
                        r#"(function(){var b=document.querySelector('[data-cmd="probe"]'); if(!b) return 'no-button'; if(b.disabled) return 'button-disabled'; b.click(); return 'clicked';})()"#,
                    );
                }
            }
            if self.onboard_tries > UNACKED_PUSH_LIMIT {
                self.onboard_needs_push = false;
            }
        }

        let rev = self.onboard.rev();
        let log = self.onboard.log_len();
        if !self.onboard_needs_push && rev == self.onboard_rev && log == self.onboard_log {
            return;
        }
        self.onboard_rev = rev;
        self.onboard_log = log;
        let snapshot = self.onboard.view().to_string();
        let literal = serde_json::to_string(&snapshot).unwrap_or_else(|_| "\"\"".to_owned());
        let script = format!("window.__onboard && window.__onboard({literal});");
        match self.webview.evaluate_script(&script) {
            Ok(()) => self.onboard_needs_push = false,
            // 页面还没加载完就先不记账，下一帧再推。
            Err(_) => self.onboard_needs_push = true,
        }
    }

    fn apply_onboard_effect(&mut self, window: &tao::window::Window, effect: onboard::Effect) {
        match effect {
            onboard::Effect::None => {}
            onboard::Effect::StartHost => {
                self.onboard_needs_push = false;
                self.start_host(window);
            }
            onboard::Effect::OpenLogs => self.open_logs(),
            onboard::Effect::Quit => {
                self.persist_window_state(window);
                self.shutdown();
                std::process::exit(0);
            }
        }
    }

    fn persist_window_state(&self, window: &tao::window::Window) {
        // 存逻辑像素：物理像素在 2x/混合 DPI 之间搬移会在下次启动时放大一倍。
        let scale = window.scale_factor();
        let size = window.inner_size().to_logical::<f64>(scale);
        let position = window
            .outer_position()
            .map(|p| p.to_logical::<f64>(scale))
            .unwrap_or_default();
        let state = WindowState {
            x: position.x.round() as i32,
            y: position.y.round() as i32,
            width: size.width.round() as u32,
            height: size.height.round() as u32,
            maximized: window.is_maximized(),
        }
        .sanitized();
        if let Err(err) = save_window_state(&state) {
            eprintln!("dsh-desktop: 保存窗口状态失败：{err}");
        }
    }

    fn shutdown(&mut self) {
        if let Some(mut host) = self.host.take() {
            host.stop();
        }
        let _ = std::fs::remove_file(host_pid_file());
        if let Some(instance) = self.instance.take() {
            instance.release();
        }
        wipe_webview_storage();
    }
}

/// `deviceInfo()` 的返回值：给前端显示用的一行摘要。
fn device_info() -> String {
    let pretty = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|body| {
            body.lines()
                .find_map(|line| line.strip_prefix("PRETTY_NAME=").map(str::to_owned))
        })
        .map(|name| name.trim_matches('"').to_owned())
        .unwrap_or_else(|| "Linux".to_owned());

    let arch = std::env::consts::ARCH;
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());

    format!(
        "dsh-desktop {} · {pretty} · {arch} · {desktop}",
        env!("CARGO_PKG_VERSION")
    )
}

/// 原生目录选择（rfd：Linux 走 xdg-portal / GTK，macOS/Windows 走各自原生对话框）。
fn pick_directory() -> Option<String> {
    rfd::FileDialog::new()
        .set_title("选择工作区目录")
        .pick_folder()
        .map(|path| path.display().to_string())
}

/// 用系统默认程序打开外链（跨平台）。
fn open_external(url: &str) {
    #[cfg(target_os = "linux")]
    spawn_detached("xdg-open", &[url]);
    #[cfg(target_os = "macos")]
    spawn_detached("open", &[url]);
    #[cfg(target_os = "windows")]
    spawn_detached("cmd", &["/C", "start", "", url]);
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let _ = url;
}

fn spawn_detached(program: &str, args: &[&str]) {
    let _ = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod asset_tests {
    use crate::{ONBOARDING, SKELETON};

    /// 页面里的内联脚本一旦有语法错误，整段 JS 都不执行：页面只剩静态骨架（黑屏 / 空白引导页），
    /// 而且壳侧 `evaluate_script` 只会静默失败，很难从日志上看出来。0.1.5 的引导页就是栽在这里
    /// （一个字符串用单引号开头、双引号结尾）。这里做最小的字面量配平检查，够挡住这类手滑。
    fn unbalanced_literal(script: &str) -> Option<(usize, char)> {
        let mut chars = script.char_indices().peekable();
        let mut line = 1;
        while let Some((at, c)) = chars.next() {
            if c == '\n' {
                line += 1;
                continue;
            }
            if c == '/' {
                match chars.peek().map(|(_, next)| *next) {
                    // 行注释跳过；块注释一路跳到闭合（块注释里换行也要计数）。
                    Some('/') => {
                        for (_, next) in chars.by_ref() {
                            if next == '\n' {
                                line += 1;
                                break;
                            }
                        }
                        continue;
                    }
                    Some('*') => {
                        chars.next();
                        let mut prev = '\0';
                        for (_, next) in chars.by_ref() {
                            if next == '\n' {
                                line += 1;
                            }
                            if prev == '*' && next == '/' {
                                break;
                            }
                            prev = next;
                        }
                        continue;
                    }
                    _ => continue,
                }
            }
            if c != '\'' && c != '"' && c != '`' {
                continue;
            }
            let quote = c;
            let start = line;
            loop {
                let Some((_, next)) = chars.next() else {
                    return Some((start, quote));
                };
                match next {
                    '\\' => {
                        chars.next();
                    }
                    '\n' => line += 1,
                    _ if next == quote => break,
                    _ => {}
                }
            }
            let _ = at;
        }
        None
    }

    fn inline_scripts(html: &'static str) -> Vec<&'static str> {
        let mut out = Vec::new();
        let mut rest = html;
        while let Some(start) = rest.find("<script>") {
            let body = &rest[start + "<script>".len()..];
            let Some(end) = body.find("</script>") else {
                break;
            };
            out.push(&body[..end]);
            rest = &body[end + "</script>".len()..];
        }
        out
    }

    #[test]
    fn inline_scripts_have_balanced_string_literals() {
        for (name, html) in [("onboarding", ONBOARDING), ("skeleton", SKELETON)] {
            for script in inline_scripts(html) {
                assert!(
                    unbalanced_literal(script).is_none(),
                    "{name}.html 的内联脚本有未闭合的字符串：{:?}",
                    unbalanced_literal(script)
                );
            }
        }
    }

    #[test]
    fn onboarding_exposes_the_snapshot_hook() {
        assert!(
            ONBOARDING.contains("window.__onboard = function"),
            "引导页必须暴露 window.__onboard，壳靠它推快照"
        );
    }

    /// 页面重绘必须在内容真的变了才碰 DOM。以前每次快照都重写 `#foot`，按钮节点被
    /// 重建，mousedown/mouseup 落在不同节点上 → `click` 不触发 → 「按钮点不动」。
    #[test]
    fn the_footer_is_repainted_through_paint() {
        assert!(
            ONBOARDING.contains("paint(\"foot\""),
            "底部按钮必须走 paint(\"foot\", ...)，否则快照会把按钮节点重建掉"
        );
        assert!(
            !ONBOARDING.contains("document.getElementById(\"foot\").innerHTML"),
            "不要再直接写 #foot 的 innerHTML"
        );
    }

    /// 环境页要有「dsh 进程」卡片：当前壳连的宿主 + 孤儿列表 + 清理按钮。
    #[test]
    fn the_env_page_shows_running_hosts() {
        assert!(ONBOARDING.contains("hostsCard"), "环境页要画 dsh 进程卡片");
        assert!(ONBOARDING.contains("\"scan-hosts\""), "要有重新扫描按钮");
        assert!(ONBOARDING.contains("\"reap-hosts\""), "要有清理孤儿按钮");
        assert!(ONBOARDING.contains("S.hosts"), "快照要带 hosts 字段");
    }
}
