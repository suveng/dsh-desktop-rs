//! 宿主进程监督：spawn `dsh --profile web`，把它的 stdio 变成生命周期事件。
//!
//! 设计约束（见 `docs/architecture.md` D3/D6）：
//! - 就绪信号是 stdout 上的 `dsh web: <url>` 行 —— 上游源码注释明确这是给 supervisor 的契约。
//! - 对外只有**同步** API：事件通道 + `poll()`。本 crate 内部不引入 async runtime，
//!   UI 线程每帧 drain 一次即可，不存在 `.await` 泄漏到上层。
//! - 带 token 的 URL 不实现 `Display` 的裸输出：`Debug` 一律脱敏（R4）。

use std::fmt;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::{Duration, Instant};

/// 上游打印就绪 URL 的前缀（`packages/bundle/web-app/src/index.ts:231`）。
pub const READY_PREFIX: &str = "dsh web: ";

/// `startup.ts` 侧没有超时，这个值只是壳的耐心。
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// 退出后最多等这么久，让 stdio 读取线程把最后几行交出来。
const PUMP_DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

/// 优雅关闭窗口：先 SIGTERM 整个进程组，超时再 SIGKILL。
pub const DEFAULT_STOP_GRACE: Duration = Duration::from_secs(10);

/// 日志流来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// 宿主生命周期事件。
#[derive(Debug)]
pub enum Event {
    /// 子进程已创建。
    Starting { pid: u32 },
    /// 拿到就绪 URL。只会出现一次。
    Ready(ReadyUrl),
    /// 一行日志（已脱敏）。
    Log { stream: Stream, line: String },
    /// 子进程退出。
    Exited { code: Option<i32> },
    /// 监督层判定失败（超时、无法解析等）。之后不会再有就绪事件。
    Failed(String),
    /// `stop()` 完成。
    Stopped,
}

/// 带鉴权 token 的本地 URL。
///
/// `Debug` / `Display` 都只输出脱敏形式；要拿真实值必须显式调 `as_str()`，
/// 这样 grep `format!("{:?}", url)` 之类的意外泄漏会在 code review 里立刻暴露。
#[derive(Clone, PartialEq, Eq)]
pub struct ReadyUrl(String);

impl ReadyUrl {
    /// 解析一行 `dsh web: http://127.0.0.1:PORT/?token=...`。
    ///
    /// 只接受 `http(s)://` 开头；空 token 也接受（上游未来可能改成 cookie-only）。
    pub fn parse(line: &str) -> Option<Self> {
        let rest = line.trim().strip_prefix(READY_PREFIX)?.trim();
        if !(rest.starts_with("http://") || rest.starts_with("https://")) {
            return None;
        }
        Some(Self(rest.to_owned()))
    }

    /// 真实 URL。**不要**写进日志、窗口标题或磁盘。
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `scheme://authority` 部分，可安全展示。
    #[must_use]
    pub fn origin(&self) -> &str {
        let s = &self.0;
        let scheme_end = s.find("://").map_or(0, |i| i + 3);
        match s[scheme_end..].find('/') {
            Some(i) => &s[..scheme_end + i],
            None => s,
        }
    }

    /// 脱敏后的可展示形式。
    #[must_use]
    pub fn redacted(&self) -> String {
        format!("{}/?token=<redacted>", self.origin())
    }
}

impl fmt::Debug for ReadyUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ReadyUrl(")?;
        f.write_str(&self.redacted())?;
        f.write_str(")")
    }
}

impl fmt::Display for ReadyUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.redacted())
    }
}

/// 把任意文本里 `token=<value>` 的值遮掉，用于转发宿主日志。
#[must_use]
pub fn redact(line: &str) -> String {
    const KEY: &str = "token=";
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(idx) = rest.find(KEY) {
        let (head, tail) = rest.split_at(idx + KEY.len());
        out.push_str(head);
        let end = tail
            .find(['&', ' ', '\t', '"', '\'', '\\'])
            .unwrap_or(tail.len());
        out.push_str("<redacted>");
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// 如何拉起宿主。
#[derive(Clone, Debug)]
pub struct LaunchSpec {
    program: PathBuf,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    /// 额外注入的环境变量。键名大小写敏感，同名的后设值覆盖先设值。
    envs: Vec<(String, String)>,
    ready_timeout: Duration,
    stop_grace: Duration,
}

impl LaunchSpec {
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: None,
            envs: Vec::new(),
            ready_timeout: DEFAULT_READY_TIMEOUT,
            stop_grace: DEFAULT_STOP_GRACE,
        }
    }

    /// `dsh --profile web` 的标准形状。
    #[must_use]
    pub fn web(program: impl Into<PathBuf>) -> Self {
        Self::new(program).args([
            "--profile",
            "web",
            "--host",
            "127.0.0.1",
            "--port",
            "0",
            "--no-open",
        ])
    }

    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// 把参数插到最前面。
    ///
    /// Windows 上 dsh 是个 `dsh.cmd` 包装脚本；为了直接 spawn `node.exe`，
    /// 真正的入口脚本必须排在 `--profile` 这类业务参数之前。
    #[must_use]
    pub fn args_front<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut front: Vec<String> = args.into_iter().map(Into::into).collect();
        front.append(&mut self.args);
        self.args = front;
        self
    }

    /// 只给测试看：`Host::spawn` 之前核对参数顺序。
    #[cfg(test)]
    fn args_snapshot(&self) -> &[String] {
        &self.args
    }

    #[must_use]
    pub fn cwd(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// 追加一个环境变量。自动装出来的 node 靠它把 bin 目录塞进宿主进程的 PATH。
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.envs.push((key.into(), value.into()));
        self
    }

    /// 批量追加环境变量（例如 install 的代理变量）。
    #[must_use]
    pub fn envs<I, K, V>(mut self, envs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.envs
            .extend(envs.into_iter().map(|(k, v)| (k.into(), v.into())));
        self
    }

    #[must_use]
    pub fn extra_env(&self) -> &[(String, String)] {
        &self.envs
    }

    #[must_use]
    pub fn ready_timeout(mut self, timeout: Duration) -> Self {
        self.ready_timeout = timeout;
        self
    }

    #[must_use]
    pub fn stop_grace(mut self, grace: Duration) -> Self {
        self.stop_grace = grace;
        self
    }

    #[must_use]
    pub fn program(&self) -> &PathBuf {
        &self.program
    }

    /// 命令行，脱敏后可用于诊断。
    #[must_use]
    pub fn command_line(&self) -> String {
        let mut s = self.program.display().to_string();
        for a in &self.args {
            s.push(' ');
            s.push_str(a);
        }
        redact(&s)
    }
}

/// 一个被监督的宿主进程。
///
/// `Drop` 会调用 `stop()`，所以「窗口关了但宿主还在」需要显式 `mem::forget` 才做得到 ——
/// 这是刻意的，孤儿进程比多跑一次关闭流程要糟得多。
pub struct Host {
    child: Child,
    pid: u32,
    events: Receiver<Event>,
    ready: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
    /// 还活着的 stdio 读取线程数。退出时要等它们把最后几行吐干净。
    /// 还活着的 stdio 读取线程数。退出时要等它们把最后几行吐干净。
    pumps: Arc<AtomicUsize>,
    spec: LaunchSpec,
    stop_done: bool,
}

impl Host {
    /// 拉起源宿主。返回时子进程已创建，就绪事件稍后从 `poll()` 里出来。
    pub fn spawn(spec: LaunchSpec) -> std::io::Result<Self> {
        let mut cmd = Command::new(&spec.program);
        dsh_platform::quiet(&mut cmd);
        cmd.args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = &spec.cwd {
            cmd.current_dir(dir);
        }
        for (key, value) in &spec.envs {
            cmd.env(key, value);
        }

        #[cfg(unix)]
        {
            // SAFETY: 只在 fork 与 exec 之间跑，只调用 async-signal-safe 的 libc 函数。
            unsafe {
                use std::os::unix::process::CommandExt as _;
                cmd.pre_exec(|| {
                    // 独立进程组：关闭时可以对整个树一次性发信号。
                    if libc::setpgid(0, 0) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    // 壳被 kill -9 时宿主也随之退出，避免孤儿（Linux 专有）。
                    #[cfg(target_os = "linux")]
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }

        let mut child = cmd.spawn()?;
        let pid = child.id();
        let (tx, rx) = channel::<Event>();
        let ready = Arc::new(AtomicBool::new(false));
        let exited = Arc::new(AtomicBool::new(false));
        let pumps = Arc::new(AtomicUsize::new(0));

        let _ = tx.send(Event::Starting { pid });

        if let Some(out) = child.stdout.take() {
            pumps.fetch_add(1, Ordering::Relaxed);
            pump(
                out,
                Stream::Stdout,
                tx.clone(),
                Arc::clone(&ready),
                Arc::clone(&pumps),
            );
        }
        if let Some(err) = child.stderr.take() {
            pumps.fetch_add(1, Ordering::Relaxed);
            pump(
                err,
                Stream::Stderr,
                tx.clone(),
                Arc::clone(&ready),
                Arc::clone(&pumps),
            );
        }

        spawn_ready_watchdog(
            spec.ready_timeout,
            Arc::clone(&ready),
            Arc::clone(&exited),
            tx,
        );

        Ok(Self {
            child,
            pid,
            events: rx,
            ready,
            exited,
            pumps,
            spec,
            stop_done: false,
        })
    }

    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    #[must_use]
    pub fn spec(&self) -> &LaunchSpec {
        &self.spec
    }

    /// drain 所有已到达的事件；顺带检查子进程是否已退出。
    ///
    /// 调用方每帧调一次即可（GUI 主循环 / 测试轮询都适用）。
    pub fn poll(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(ev) = self.events.try_recv() {
            out.push(ev);
        }
        if !self.exited.load(Ordering::Relaxed) {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.exited.store(true, Ordering::Relaxed);
                    // 否则「崩溃原因」那行 stderr 会赶在 Exited 之后到达，被调用方丢掉。
                    let deadline = Instant::now() + PUMP_DRAIN_TIMEOUT;
                    while Instant::now() < deadline && self.pumps.load(Ordering::Relaxed) > 0 {
                        while let Ok(ev) = self.events.try_recv() {
                            out.push(ev);
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    while let Ok(ev) = self.events.try_recv() {
                        out.push(ev);
                    }
                    out.push(Event::Exited {
                        code: exit_code(&status),
                    });
                }
                Ok(None) => {}
                Err(err) => out.push(Event::Failed(format!("等待宿主退出失败：{err}"))),
            }
        }
        out
    }

    /// 阻塞等一个事件（测试与启动期用）。
    pub fn recv_timeout(&self, timeout: Duration) -> Option<Event> {
        self.events.recv_timeout(timeout).ok()
    }

    /// 优雅关闭：SIGTERM 进程组 → 等 `stop_grace` → SIGKILL 进程组。
    pub fn stop(&mut self) {
        if self.stop_done {
            return;
        }
        self.stop_done = true;

        #[cfg(unix)]
        signal_group(self.pid, libc::SIGTERM);
        // Windows 没有「礼貌地请你退出」这回事：`Child::kill` 就是 TerminateProcess，
        // 立刻生效。所以别在那儿空等满 grace（退出时会明显卡一下）。
        // 但 TerminateProcess 只管一个进程：宿主会派生子进程（pnpm、MCP 服务器、
        // 插件里的子任务），只杀父进程会留下孤儿子进程继续占内存 —— 用 `taskkill /T`
        // 整棵树收掉。
        #[cfg(windows)]
        {
            if !kill_tree(self.pid) {
                let _ = self.child.kill();
            }
        }

        let deadline = Instant::now() + self.spec.stop_grace;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => thread::sleep(Duration::from_millis(50)),
                Err(_) => break,
            }
        }

        // 直接子进程可能已退出、组里还有孙进程；也可能反过来。两者都补一发。
        if self.child.try_wait().ok().flatten().is_none() {
            #[cfg(unix)]
            signal_group(self.pid, libc::SIGKILL);
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        #[cfg(unix)]
        signal_group(self.pid, libc::SIGKILL);

        self.exited.store(true, Ordering::Relaxed);
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.stop();
    }
}

fn exit_code(status: &ExitStatus) -> Option<i32> {
    status.code()
}

fn pump<R>(
    reader: R,
    stream: Stream,
    tx: Sender<Event>,
    ready: Arc<AtomicBool>,
    pumps: Arc<AtomicUsize>,
) where
    R: std::io::Read + Send + 'static,
{
    thread::spawn(move || {
        let buf = BufReader::new(reader);
        for line in buf.lines() {
            let Ok(line) = line else { break };
            if !ready.load(Ordering::Relaxed)
                && let Some(url) = ReadyUrl::parse(&line)
            {
                ready.store(true, Ordering::Relaxed);
                let _ = tx.send(Event::Log {
                    stream,
                    line: url.redacted(),
                });
                let _ = tx.send(Event::Ready(url));
                continue;
            }
            let _ = tx.send(Event::Log {
                stream,
                line: redact(&line),
            });
        }
        pumps.fetch_sub(1, Ordering::Relaxed);
    });
}

fn spawn_ready_watchdog(
    timeout: Duration,
    ready: Arc<AtomicBool>,
    exited: Arc<AtomicBool>,
    tx: Sender<Event>,
) {
    thread::spawn(move || {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if ready.load(Ordering::Relaxed) || exited.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        if ready.load(Ordering::Relaxed) || exited.load(Ordering::Relaxed) {
            return;
        }
        let _ = tx.send(Event::Failed(format!(
            "宿主在 {} 秒内没有打印就绪 URL（`{READY_PREFIX}…`）。可能是不兼容的 dsh 版本，或 Profile 初始化失败。",
            timeout.as_secs()
        )));
    });
}

/// 杀掉整棵进程树。返回是否真的把 `taskkill` 跑成功。
///
/// Windows 的 `TerminateProcess` 没有「进程组」概念（POSIX 的 `setpgid` + 负 pid
/// 那套在 Win32 不存在），`taskkill /T` 走系统级父子关系遍历，是等价的收尾手段。
#[cfg(windows)]
#[must_use]
pub fn kill_tree(pid: u32) -> bool {
    if pid <= 1 {
        return false;
    }
    let mut cmd = Command::new("taskkill");
    cmd.args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    dsh_platform::quiet(&mut cmd);
    cmd.status().map(|status| status.success()).unwrap_or(false)
}

#[cfg(unix)]
#[must_use]
pub fn kill_tree(pid: u32) -> bool {
    signal_group(pid, libc::SIGKILL);
    true
}

#[cfg(unix)]
fn signal_group(pid: u32, sig: i32) {
    // SAFETY: kill 只读两个整数参数；负 pid 表示「发给这个进程组」。
    unsafe {
        libc::kill(-(pid as i32), sig);
    }
}

#[cfg(test)]
mod tests {
    use super::LaunchSpec;
    use std::path::PathBuf;

    /// Windows 上要 spawn `node.exe <bin.js> --profile web`：入口脚本必须排在
    /// 业务参数前面，否则 node 会把 `--profile` 当成自己的参数。
    #[test]
    fn front_args_land_before_the_business_args() {
        let spec = LaunchSpec::new(PathBuf::from("/prefix/node.exe"))
            .args(["--profile", "web"])
            .args_front(["/prefix/lib/bin.js"]);
        assert_eq!(
            spec.args_snapshot(),
            ["/prefix/lib/bin.js", "--profile", "web"]
        );
    }

    #[test]
    fn front_args_of_an_empty_spec_are_just_the_args() {
        let spec = LaunchSpec::new(PathBuf::from("dsh")).args_front(["--version"]);
        assert_eq!(spec.args_snapshot(), ["--version"]);
    }
}
