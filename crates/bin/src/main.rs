//! dsh-desktop —— DeepSeek Harness 的 Linux 桌面壳。
//!
//! 壳只做四件事（`docs/architecture.md` D1）：拉起宿主进程、开一个 webview 加载它、
//! 提供原生窗口能力、在它挂掉时给出可操作的诊断。UI 与协议都归上游。

mod desktop;
mod menu;
mod update;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tao::dpi::{LogicalPosition, LogicalSize};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::platform::unix::WindowExtUnix as _;
use tao::window::WindowBuilder;
use wry::http::Request;
use wry::{NewWindowResponse, WebViewBuilder, WebViewBuilderExtUnix as _};

use dsh_platform::instance::{self, Claim, Command, Instance};
use dsh_platform::{WindowState, load_window_state, save_window_state, state_dir};
use dsh_supervisor::{Event as HostEvent, Host, LaunchSpec};

const APP_TITLE: &str = "DeepSeek Harness";
const SKELETON: &str = include_str!("../assets/skeleton.html");
const FAILURE: &str = include_str!("../assets/failure.html");
const SHIM: &str = include_str!("../assets/shim.js");

/// 轮询宿主的间隔。10 Hz 足够让「就绪 → 加载页面」看起来是即时的，又不会空转。
const TICK: Duration = Duration::from_millis(100);

/// 最多保留多少行宿主日志用于诊断。
const LOG_TAIL: usize = 200;

/// 连续崩溃几次之后不再自动重启（M2 契约：重启 2 次后进失败页）。
const MAX_AUTO_RESTARTS: u32 = 2;

/// SIGTERM / SIGINT / SIGHUP 置位。信号处理器里只允许写这个原子量。
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// 桌面环境注销、systemd 停止、`kill` 都走这条路。
///
/// 没有它的话，壳被 SIGTERM 打断时 `Drop` 不会跑，宿主就会变成孤儿 ——
/// 这一点在本机（无窗口管理器）实测过。
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

fn main() {
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

    let code = if cli.probe {
        run_probe(&cli)
    } else {
        run_window(cli)
    };
    std::process::exit(code);
}

// ---------------------------------------------------------------- 命令行

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
    /// 无窗口：只查一次更新并打印 JSON。
    check_update: bool,
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

/// `dsh --profile web` 的标准参数（`startup.ts:64-66`）。
const STANDARD_ARGS: [&str; 7] = [
    "--profile",
    "web",
    "--host",
    "127.0.0.1",
    "--port",
    "0",
    "--no-open",
];

/// 宿主命令：可执行文件 + 插在标准参数之前的参数。
fn host_command(cli: &Cli) -> (PathBuf, Vec<String>) {
    if let Some(parts) = &cli.host_cmd {
        return (PathBuf::from(&parts[0]), parts[1..].to_vec());
    }
    if let Some(path) = &cli.host_program {
        return (path.clone(), Vec::new());
    }
    // 源码工作区里 `dsh` 全局装的往往是旧版（0.1.x）。工作区编译产物既新又快
    // （就绪 785ms vs `pnpm dsh` 1885ms），所以它优先于 PATH 里的同名命令。
    if let Some(entry) = compiled_harness_entry(cli) {
        return (PathBuf::from("node"), vec![entry.display().to_string()]);
    }
    if let Some(path) = which("dsh") {
        return (path, Vec::new());
    }
    (PathBuf::from("pnpm"), vec!["dsh".to_owned()])
}

/// 支持区间的下界（`architecture.md` D7）。
const MIN_HOST_VERSION: &str = "0.2.2-alpha.2";

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
    let in_minor = {
        let mut parts = version.split('.');
        parts.next() == Some("0") && parts.next() == Some("2")
    };
    let at_least_min = !update::is_newer(MIN_HOST_VERSION, version);
    if in_minor && at_least_min {
        None
    } else {
        Some(format!(
            "dsh {version} 不在支持区间（{MIN_HOST_VERSION} ≤ v < 0.3）"
        ))
    }
}

/// 把 CLI 变成一份启动说明。
fn launch_spec(cli: &Cli) -> LaunchSpec {
    let (program, prefix) = host_command(cli);

    let mut spec = LaunchSpec::new(program).args(prefix).args(STANDARD_ARGS);
    if !cli.host_args.is_empty() {
        spec = spec.args(cli.host_args.clone());
    }
    if let Some(cwd) = &cli.host_cwd {
        spec = spec.cwd(cwd);
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
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

// ---------------------------------------------------------------- 无窗口探针

fn run_probe(cli: &Cli) -> i32 {
    let spec = launch_spec(cli);
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

// ---------------------------------------------------------------- 窗口

fn run_window(cli: Cli) -> i32 {
    install_signal_handlers();

    // D5：单实例先于一切。第二个进程只负责把请求转过去，然后退出。
    let instance = match Instance::claim() {
        Ok(Claim::Primary(instance)) => instance,
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

/// R4：带 token 的 URL 不能**落盘**。
///
/// WebKit 会把子资源连同带 `?token=` 的 Referer 写进磁盘缓存，日志脱敏救不了它。
/// 做法是把 XDG 缓存/数据目录指到 `$XDG_RUNTIME_DIR`（tmpfs），并在启动与退出时各清一次。
fn isolate_webview_storage() -> std::path::PathBuf {
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

fn wipe_webview_storage() {
    let _ = std::fs::remove_dir_all(webview_home());
}

fn run_window_inner(cli: Cli, instance: Option<Instance>) -> i32 {
    let spec = launch_spec(&cli);

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
    if let Err(err) = menubar.install(window.gtk_window(), window.default_vbox()) {
        eprintln!("dsh-desktop: 安装原生菜单失败：{err}");
    }

    let ipc: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let ipc_sink = Arc::clone(&ipc);
    let drops: Arc<desktop::DropPaths> = Arc::new(desktop::DropPaths::new());
    let drops_sink = Arc::clone(&drops);

    let vbox = window.default_vbox().expect("tao 的 default_vbox 缺失");
    let webview = WebViewBuilder::new()
        .with_html(SKELETON)
        .with_initialization_script(SHIM)
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
                spawn_detached("xdg-open", &[url.as_str()]);
            }
            internal
        })
        .with_new_window_req_handler(|url, _features| {
            if url.starts_with("http://127.0.0.1") || url.starts_with("http://localhost") {
                return NewWindowResponse::Allow;
            }
            spawn_detached("xdg-open", &[url.as_str()]);
            NewWindowResponse::Deny
        })
        .build_gtk(vbox)
        .expect("创建 webview 失败");

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
        logs: VecDeque::with_capacity(LOG_TAIL),
        ready: false,
        failure: None,
        ipc,
        webview,
        restarts: 0,
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
    app.start_host(&window);

    event_loop.run(move |event, _target, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + TICK);
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
    logs: VecDeque<String>,
    ready: bool,
    failure: Option<(String, String)>,
    ipc: Arc<Mutex<Vec<String>>>,
    webview: wry::WebView,
    restarts: u32,
}

impl App {
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
        self.ready = false;
        self.failure = None;
        self.logs.clear();
        self.webview.load_html(SKELETON).ok();
        window.set_title(&self.title_for(&format!("{APP_TITLE} — 正在启动…")));
        eprintln!("dsh-desktop: 启动 {}", self.spec.command_line());

        match Host::spawn(self.spec.clone()) {
            Ok(host) => self.host = Some(host),
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
        self.drain_menu(window);
        self.drain_commands(window);
        self.drain_desktop(window);

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
                    self.ready = true;
                    self.origin = Some(url.origin().to_owned());
                    self.ready_since = Some(Instant::now());
                    let target = self.navigate_deep_link();
                    self.start_update_check();
                    self.strip_token_from_location();
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
                    spawn_detached("xdg-open", &[&dir.display().to_string()]);
                }
                "quit" => {
                    self.persist_window_state(window);
                    self.shutdown();
                    std::process::exit(0);
                }
                other if other.starts_with('{') => self.handle_shim(other),
                other => eprintln!("dsh-desktop: 未知的界面指令 {other:?}"),
            }
        }
    }

    /// 首屏加载后把 `?token=` 从地址里去掉（WebKit 的 Referer 也就干净了）。
    fn strip_token_from_location(&mut self) {
        self.webview
            .evaluate_script("history.replaceState(null, '', '/');")
            .ok();
    }

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

    fn show_message(&self, window: &tao::window::Window, title: &str, body: &str) {
        use gtk::prelude::{DialogExt as _, GtkWindowExt as _, ObjectExt as _};
        let dialog = gtk::MessageDialog::new(
            Some(window.gtk_window()),
            gtk::DialogFlags::MODAL,
            gtk::MessageType::Info,
            gtk::ButtonsType::Close,
            title,
        );
        dialog.set_property("secondary-text", body);
        dialog.run();
        dialog.close();
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
                if let Some(state) = self.selftest.as_mut() {
                    state.results.push(value.to_owned());
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
        let has_host_path = self.simulate_drop.is_empty()
            || state.results.iter().any(|row| row.contains("\"hostPath\""));
        if !has_device || !has_host_path {
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

    fn drain_menu(&mut self, window: &tao::window::Window) {
        for action in self.menubar.drain_events() {
            match action {
                menu::Action::Reload => {
                    if let Err(err) = self.webview.reload() {
                        eprintln!("dsh-desktop: 重新加载失败：{err}");
                    }
                }
                menu::Action::OpenLogs => self.open_logs(),
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
                    window.set_visible(true);
                    window.set_focus();
                }
                Command::OpenUrl(url) => {
                    self.open_url = Some(url);
                    if self.ready {
                        let _ = self.navigate_deep_link();
                    }
                    self.start_update_check();
                    self.strip_token_from_location();
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
        spawn_detached("xdg-open", &[&dir.display().to_string()]);
    }

    fn show_about(&self, window: &tao::window::Window) {
        use gtk::prelude::{DialogExt as _, GtkWindowExt as _, ObjectExt as _};
        let dialog = gtk::MessageDialog::new(
            Some(window.gtk_window()),
            gtk::DialogFlags::MODAL,
            gtk::MessageType::Info,
            gtk::ButtonsType::Close,
            "dsh-desktop",
        );
        dialog.set_property(
            "secondary-text",
            format!(
                "版本 {}\nDeepSeek Harness 的 Linux 桌面壳。\n界面与协议归上游，壳只负责窗口、进程与桌面集成。\n\n快捷键\n{}",
                env!("CARGO_PKG_VERSION"),
                self.menubar.shortcuts_help()
            ),
        );
        dialog.run();
        dialog.close();
    }

    fn fail(&mut self, window: &tao::window::Window, summary: &str, detail: &str) {
        desktop::notify(summary, detail);
        self.failure = Some((summary.to_owned(), detail.to_owned()));
        window.set_title(&format!("{APP_TITLE} — 启动失败"));
        self.webview
            .load_html(&failure_page(summary, detail, &self.logs, &self.spec))
            .ok();
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
        if let Some(instance) = self.instance.take() {
            instance.release();
        }
        wipe_webview_storage();
    }
}

/// 失败页：一句话结论 + 可复制的诊断 + 三个按钮（`architecture.md` §8.3）。
fn failure_page(summary: &str, detail: &str, logs: &VecDeque<String>, spec: &LaunchSpec) -> String {
    let mut diag = String::new();
    diag.push_str("命令: ");
    diag.push_str(&spec.command_line());
    diag.push_str("\n\n");
    diag.push_str(detail);
    if !logs.is_empty() {
        diag.push_str("\n\n--- 宿主日志（最后 40 行）---\n");
        for line in logs
            .iter()
            .rev()
            .take(40)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            diag.push_str(line);
            diag.push('\n');
        }
    }

    FAILURE
        .replace(
            "id=\"summary\"></p>",
            &format!("id=\"summary\">{}</p>", escape(summary)),
        )
        .replace(
            "id=\"detail\"></pre>",
            &format!("id=\"detail\">{}</pre>", escape(&diag)),
        )
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
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

/// 原生目录选择。用 GTK 的对话框：它已经是依赖，且和 webview 同一个 toolkit。
fn pick_directory() -> Option<String> {
    use gtk::prelude::{DialogExt as _, FileChooserExt as _, GtkWindowExt as _};

    let dialog = gtk::FileChooserDialog::new(
        Some("选择工作区目录"),
        None::<&gtk::Window>,
        gtk::FileChooserAction::SelectFolder,
    );
    dialog.add_button("取消", gtk::ResponseType::Cancel);
    dialog.add_button("选择", gtk::ResponseType::Accept);
    let response = dialog.run();
    let path = if response == gtk::ResponseType::Accept {
        dialog.filename().map(|p| p.display().to_string())
    } else {
        None
    };
    dialog.close();
    path
}

fn spawn_detached(program: &str, args: &[&str]) {
    let _ = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
