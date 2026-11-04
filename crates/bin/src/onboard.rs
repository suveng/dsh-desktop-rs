//! 安装引导的状态机与执行器。
//!
//! 分工：这个模块是**唯一的状态源**，界面（`assets/onboarding.html`）只负责渲染
//! [`Onboard::view`] 给出的 JSON，并把点击变成命令回传。这样刷新页面、丢了消息都不影响正确性。
//!
//! 执行有两档：
//! - [`Job`]：长任务（npm / pnpm / curl），逐行日志 + 可取消。
//! - 后台线程：短任务（解压、校验、备份、还原、拉版本列表），一个 `Result` 回来就行。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use dsh_setup::envcheck::{self, DshRuntime, NodeRuntime};
use dsh_setup::install::{Job, Stream, run_blocking};
use dsh_setup::profiles::{self, ProfileInfo};
use dsh_setup::settings::{self, FALLBACK_NODE_VERSION, InstallScope, Settings};
use dsh_setup::{node, stamp, version::Version};
use serde_json::{Value, json};

/// 引导页用到的目录。
#[derive(Clone, Debug)]
pub struct Paths {
    /// `~/.local/share/dsh-desktop-rs`
    pub data: PathBuf,
    /// `~/.local/state/dsh-desktop-rs`
    pub state: PathBuf,
    /// `~/.dsh`
    pub dsh_home: PathBuf,
}

impl Paths {
    /// 按当前用户的 XDG 目录算一份。
    #[must_use]
    pub fn from_platform() -> Self {
        Self {
            data: dsh_platform::data_dir(),
            state: dsh_platform::state_dir(),
            dsh_home: profiles::dsh_home(),
        }
    }

    /// 用户前缀：`npm install -g --prefix <data>/host` 的落点。
    #[must_use]
    pub fn user_prefix(&self) -> PathBuf {
        self.data.join("host")
    }

    #[must_use]
    pub fn user_dsh(&self) -> PathBuf {
        self.user_prefix().join("bin").join("dsh")
    }

    #[must_use]
    pub fn backups(&self) -> PathBuf {
        self.state.join("backups")
    }

    /// 下载暂存目录（node tarball 与 SHASUMS256），装完即删。
    #[must_use]
    pub fn install_downloads(&self) -> PathBuf {
        self.data.join("downloads")
    }

    #[must_use]
    pub fn profiles_root(&self) -> PathBuf {
        profiles::profiles_root(&self.dsh_home)
    }
}

/// 引导页步骤。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Env,
    Proxy,
    Version,
    Profile,
    Plugins,
    Progress,
    Done,
}

impl Screen {
    fn key(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::Proxy => "proxy",
            Self::Version => "version",
            Self::Profile => "profile",
            Self::Plugins => "plugins",
            Self::Progress => "progress",
            Self::Done => "done",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Some(match key {
            "env" => Self::Env,
            "proxy" => Self::Proxy,
            "version" => Self::Version,
            "profile" => Self::Profile,
            "plugins" => Self::Plugins,
            "progress" => Self::Progress,
            "done" => Self::Done,
            _ => return None,
        })
    }
}

/// 环境检出的快照。界面顶部那三行就靠它。
#[derive(Clone, Debug)]
pub struct EnvReport {
    pub node: NodeRuntime,
    pub node_bin: Option<PathBuf>,
    pub dsh: Option<DshRuntime>,
    pub profiles: Vec<ProfileInfo>,
    /// 装过的 node 版本（`data/node/v*`）。
    pub installed_nodes: Vec<String>,
}

impl EnvReport {
    #[must_use]
    pub fn web_profiles(&self) -> Vec<String> {
        self.profiles
            .iter()
            .filter(|profile| profile.web_capable)
            .map(|profile| profile.name.clone())
            .collect()
    }

    /// 能不能直接起宿主。
    ///
    /// 一台全新机器上一个 profile 也没有——这不表示不能启动：dsh 首次带 `--profile web`
    /// 起来时会按官方模板建一个，所以“空目录”算就绪。只有“有 profile、但没有一个带 Web App”
    /// 才真要用户先去处理。
    #[must_use]
    pub fn ready(&self, settings: &Settings) -> bool {
        self.node.is_ready()
            && self.dsh.as_ref().is_some_and(DshRuntime::supported)
            && (self.profiles.is_empty()
                || self
                    .web_profiles()
                    .iter()
                    .any(|name| name == settings.target_profile()))
    }
}

/// 引导页要对壳做的事。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    None,
    /// 环境就绪（装完了 / 用户点了「启动」）。
    StartHost,
    Quit,
    /// 打开日志目录。
    OpenLogs,
}

/// 一条日志。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLine {
    pub kind: &'static str,
    pub text: String,
}

/// 一个执行步骤。
#[derive(Clone, Debug)]
enum Step {
    /// 先取 SHASUMS256.txt，后面的校验要有依据。
    FetchShasums {
        version: String,
    },
    InstallNode {
        version: String,
    },
    /// 解压前核 SHA-256：网络上的 tar 包不能直接往用户目录里铺。
    VerifyNode {
        version: String,
    },
    ExtractNode {
        version: String,
    },
    InstallDsh {
        version: String,
        scope: InstallScope,
    },
    /// dsh 装完之后的推荐插件：用的是 dsh 自己的 `plugin add`（内部转发 pnpm）。
    InstallPlugins {
        profile: String,
        packages: Vec<String>,
    },
    RepairDeps {
        profile: String,
    },
    Backup {
        profile: String,
    },
    Restore {
        archive: PathBuf,
        profile: String,
    },
    Import {
        archive: PathBuf,
        name: String,
    },
    FetchVersions,
    Reprobe,
}

impl Step {
    fn label(&self) -> String {
        match self {
            Self::FetchShasums { version } => format!("获取 node v{version} 的校验清单"),
            Self::InstallNode { version } => format!("下载 node v{version}"),
            Self::VerifyNode { version } => format!("校验 node v{version} 的 SHA-256"),
            Self::ExtractNode { version } => format!("解压并自检 node v{version}"),
            Self::InstallDsh { version, scope } => match scope {
                InstallScope::User => format!("安装 dsh {version}（用户目录）"),
                _ => format!("安装 dsh {version}"),
            },
            Self::RepairDeps { profile } => format!("重建 profile「{profile}」的依赖"),
            Self::InstallPlugins { packages, .. } => {
                format!("预装推荐插件（{} 个）", packages.len())
            }
            Self::Backup { profile } => format!("备份 profile「{profile}」"),
            Self::Restore { profile, .. } => format!("还原 profile「{profile}」"),
            Self::Import { name, .. } => format!("导入 profile「{name}」"),
            Self::FetchVersions => "获取可用版本".to_owned(),
            Self::Reprobe => "重新检测环境".to_owned(),
        }
    }
}

enum Running {
    Idle,
    Job {
        job: Job,
        step: Step,
    },
    Worker {
        rx: Receiver<Result<String, String>>,
        step: Step,
    },
}

/// 引导页本体。
pub struct Onboard {
    paths: Paths,
    pub settings: Settings,
    env: EnvReport,
    screen: Screen,
    visible: bool,
    banner: Option<(String, String)>,
    error: Option<String>,
    notice: Option<String>,
    log: Vec<LogLine>,
    log_open: bool,
    pending: VecDeque<Step>,
    running: Running,
    /// 忙的时候收到的「开始安装 / 更新」请求：队列空了再重放，免得点击被默默丢掉。
    queued: Option<Value>,
    /// 版本下拉里的候选。
    versions: Vec<String>,
    /// `npm view` 拿到过就不再拉（除非用户点刷新）。
    versions_fetched: bool,
    /// 这一轮装完是否要自动启动宿主（见 `Step::Reprobe` 的成功分支）。
    autostart: bool,
    /// 结构变化计数：界面据此决定要不要整块重画。
    rev: u64,
    /// 点过几次「重新检测」。检测结果没变时界面也要有变化，否则用户以为点击没生效。
    probes: u32,
    /// 进程表里扫到的 dsh 宿主（含壳自己连的那个和遗留的孤儿）。
    hosts: Vec<crate::hostscan::DshProcess>,
    /// 壳当前连着的宿主（`None` = 壳自己没起宿主，比如还在引导阶段）。
    host_pid: Option<u32>,
    host_url: Option<String>,
    host_ready: bool,
    /// 最近一次备份 / 当前选中的备份文件。
    selected_backup: Option<PathBuf>,
    /// 导入时要复制的归档（由文件选择器选好）。
    pending_archive: Option<PathBuf>,
    dirty: bool,
}

impl Onboard {
    #[must_use]
    pub fn new(paths: Paths, settings: Settings, candidates: &[envcheck::Candidate]) -> Self {
        // 候选要在这里就参与探测：PATH 上已经有一份能用的 dsh 时，不该还被判成「环境不全」。
        let env = detect(&paths, candidates, &settings);
        Self {
            paths,
            settings,
            env,
            screen: Screen::Env,
            visible: false,
            banner: None,
            error: None,
            notice: None,
            log: Vec::new(),
            log_open: true,
            pending: VecDeque::new(),
            running: Running::Idle,
            queued: None,
            versions: Vec::new(),
            versions_fetched: false,
            autostart: false,
            rev: 0,
            probes: 0,
            hosts: Vec::new(),
            host_pid: None,
            host_url: None,
            host_ready: false,
            selected_backup: None,
            pending_archive: None,
            dirty: true,
        }
    }

    /// 打开引导页（或换一个失败原因重新打开）。
    pub fn open(&mut self, candidates: &[envcheck::Candidate], banner: Option<(String, String)>) {
        self.visible = true;
        // 下载 / 解压 / 备份都往数据目录写，全新机器上这些目录还不存在。
        for dir in [
            &self.paths.data,
            &self.paths.state,
            &self.paths.dsh_home,
            &self.paths.backups(),
        ] {
            let _ = settings::ensure_private_dir(dir);
        }
        let _ = std::fs::create_dir_all(self.paths.install_downloads());
        self.banner = banner;
        self.error = None;
        self.notice = None;
        self.refresh(candidates);
        self.refresh_hosts();
        if self.env.ready(&self.settings) {
            // 环境其实是好的（例如用户从菜单进来做版本/备份管理、或失败另有原因）：
            // 停在「完成」页——那里有环境概览、启动按钮和失败诊断，但**不自动重启**：
            // 刚点开就被夺回界面，用户连看日志的机会都没有。
            self.screen = Screen::Done;
            self.autostart = false;
        } else {
            // 环境不全：永远从第一步开始走，别停在上次的「完成」页。
            self.screen = Screen::Env;
        }
        // 版本下拉的数据是异步拉的；没拉过就顺手排一个，失败也不致命。
        if !self.versions_fetched && !self.busy() {
            self.pending.push_back(Step::FetchVersions);
        }
        self.dirty = true;
    }

    pub fn close(&mut self) {
        self.visible = false;
    }

    #[must_use]
    pub fn visible(&self) -> bool {
        self.visible
    }

    #[must_use]
    pub fn busy(&self) -> bool {
        // `queued` 也算忙：否则调用方（自检循环 / 界面）会在重放之前就停下来。
        !matches!(self.running, Running::Idle) || !self.pending.is_empty() || self.queued.is_some()
    }

    /// node / dsh / profile 三样都齐了，可以直接起宿主。
    #[must_use]
    pub fn env_ready(&self) -> bool {
        self.env.ready(&self.settings)
    }

    /// 结构版本号：界面据此决定要不要整块重画。
    #[must_use]
    pub fn rev(&self) -> u64 {
        self.rev
    }

    /// 已累积的日志行数：变化时才重新推日志面板。
    #[must_use]
    pub fn log_len(&self) -> usize {
        self.log.len()
    }

    /// 启动失败时把诊断写进日志面板（原来失败页里那段可复制文本）。
    pub fn note_failure(
        &mut self,
        summary: &str,
        detail: &str,
        command: &str,
        logs: &VecDeque<String>,
    ) {
        push_log(&mut self.log, "err", format!("启动失败：{summary}"));
        push_log(&mut self.log, "cmd", format!("命令: {command}"));
        for line in detail.lines() {
            push_log(&mut self.log, "err", line.to_owned());
        }
        if !logs.is_empty() {
            push_log(
                &mut self.log,
                "info",
                "--- 宿主日志（最后 40 行）---".to_owned(),
            );
            let tail: Vec<&String> = logs.iter().rev().take(40).collect();
            for line in tail.into_iter().rev() {
                push_log(&mut self.log, "out", line.clone());
            }
        }
        self.log_open = true;
    }

    /// 设置被界面改过之后由壳取走，落盘。
    #[must_use]
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// 重新探测环境（node / dsh / profile）。
    pub fn refresh(&mut self, candidates: &[envcheck::Candidate]) {
        self.env = detect(&self.paths, candidates, &self.settings);
        self.rev = self.rev.wrapping_add(1);
        self.dirty = true;
    }

    /// 壳告诉引导页：当前连的是哪个宿主。每帧喂一次，不动 `rev`（免得每帧重画）。
    pub fn set_host_status(&mut self, pid: Option<u32>, url: Option<String>, ready: bool) {
        if self.host_pid != pid || self.host_url != url || self.host_ready != ready {
            self.host_pid = pid;
            self.host_url = url;
            self.host_ready = ready;
            // 状态变了界面要重描，但只描这一次（`dirty` 只影响渲染，不进 `rev`）。
            self.rev = self.rev.wrapping_add(1);
        }
    }

    /// 重扫进程表（会调用 `ps`，只在打开引导页 / 点检测 / 点清理时跑）。
    pub fn refresh_hosts(&mut self) {
        self.hosts = crate::hostscan::scan();
    }

    /// 不属于「壳当前连的宿主」且已经没人管的 dsh。
    fn orphans(&self) -> Vec<&crate::hostscan::DshProcess> {
        let shell = std::process::id();
        self.hosts
            .iter()
            .filter(|proc| proc.is_orphan(shell, self.host_pid))
            .collect()
    }

    /// 每帧调一次：推进执行队列。
    pub fn tick(&mut self, candidates: &[envcheck::Candidate]) -> Effect {
        let effect = self.advance(candidates);
        if self.running_is_idle()
            && let Some(step) = self.pending.pop_front()
        {
            self.start_step(step);
        }
        // 用户点「开始安装」时可能还在拉版本列表（刚打开向导就是这样）：
        // 当时 busy 会把请求丢掉，按钮看着像坏的。这里等队列空了再重放一次。
        if self.running_is_idle()
            && self.pending.is_empty()
            && let Some(queued) = self.queued.take()
        {
            return self.handle(&queued, candidates);
        }
        effect
    }

    fn running_is_idle(&self) -> bool {
        matches!(self.running, Running::Idle)
    }

    fn advance(&mut self, candidates: &[envcheck::Candidate]) -> Effect {
        let mut finished: Option<(Step, Result<String, String>)> = None;
        match &mut self.running {
            Running::Idle => {}
            Running::Job { job, step } => {
                let outcome = job.poll();
                for (stream, line) in &outcome.lines {
                    let kind = if *stream == Stream::Stderr {
                        "err"
                    } else {
                        "out"
                    };
                    push_log(&mut self.log, kind, line.clone());
                }
                if let Some(code) = outcome.finished {
                    let step = step.clone();
                    if outcome.cancelled {
                        finished = Some((step.clone(), Err("已取消".to_owned())));
                    } else if code == Some(0) {
                        finished = Some((step, Ok(String::new())));
                    } else {
                        let tail = job.stderr_tail().to_owned();
                        finished = Some((step, Err(describe_failure(&tail, code))));
                    }
                }
            }
            Running::Worker { rx, step } => match rx.try_recv() {
                Ok(result) => finished = Some((step.clone(), result)),
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    finished = Some((step.clone(), Err("后台任务意外结束".to_owned())));
                }
            },
        }

        let Some((step, result)) = finished else {
            return Effect::None;
        };
        self.running = Running::Idle;
        self.finish(step, result, candidates)
    }

    fn finish(
        &mut self,
        step: Step,
        result: Result<String, String>,
        candidates: &[envcheck::Candidate],
    ) -> Effect {
        match result {
            Ok(note) => self.after_success(step, note, candidates),
            Err(err) => {
                // 版本列表拉不到不致命：下拉里还有内置的默认版本，不能让整个
                // 引导页停在「失败」上（node 没装时这里必然失败）。
                if matches!(step, Step::FetchVersions) {
                    push_log(&mut self.log, "warn", format!("拉取版本列表失败：{err}"));
                    self.notice = Some("拿不到在线版本列表，将使用内置默认版本".to_owned());
                    self.rev = self.rev.wrapping_add(1);
                    return Effect::None;
                }
                // 插件装不上也不能抦住主流程：dsh 已经能用，用户之后还能在插件市场里补。
                if matches!(step, Step::InstallPlugins { .. }) {
                    push_log(&mut self.log, "warn", format!("预装推荐插件失败：{err}"));
                    self.notice =
                        Some("推荐插件没装上，dsh 本身没事，可以稍后从插件市场重试".to_owned());
                    self.rev = self.rev.wrapping_add(1);
                    return Effect::None;
                }
                // 全局装不上（权限）时自动退到用户前缀 —— 这就是「优先全局」的兜底。
                if let Step::InstallDsh {
                    version,
                    scope: InstallScope::Auto | InstallScope::Global,
                } = &step
                    && looks_like_permission_problem(&err)
                {
                    push_log(
                        &mut self.log,
                        "warn",
                        "全局安装没有权限，改装到用户目录".to_owned(),
                    );
                    self.pending.push_front(Step::InstallDsh {
                        version: version.clone(),
                        scope: InstallScope::User,
                    });
                    return Effect::None;
                }
                push_log(&mut self.log, "err", err.clone());
                self.error = Some(err);
                self.pending.clear();
                self.screen = Screen::Progress;
                self.rev = self.rev.wrapping_add(1);
                Effect::None
            }
        }
    }

    fn after_success(
        &mut self,
        step: Step,
        note: String,
        candidates: &[envcheck::Candidate],
    ) -> Effect {
        if step_note_wanted(&step) && !note.is_empty() {
            for line in note.lines() {
                push_log(&mut self.log, "info", line.to_owned());
            }
        }
        match step {
            Step::FetchVersions => {
                self.ingest_versions(&note);
                self.versions_fetched = true;
                push_log(
                    &mut self.log,
                    "info",
                    format!("拿到 {} 个可选版本", self.versions.len()),
                );
            }
            Step::FetchShasums { .. } => {}
            Step::InstallNode { version } => {
                // 下载完成 → 校验 SHA-256 → 解压 + 自检（短任务，放后台线程）。
                self.pending.push_front(Step::VerifyNode { version });
            }
            Step::VerifyNode { version } => {
                self.pending.push_front(Step::ExtractNode { version });
            }
            Step::Reprobe => {
                self.refresh(candidates);
                if self.env.ready(&self.settings) && self.pending.is_empty() {
                    self.screen = Screen::Done;
                    self.rev = self.rev.wrapping_add(1);
                    // 装完不再让用户点一次：直接进主界面（失败的话会带着诊断回到这里）。
                    self.autostart = true;
                }
            }
            Step::ExtractNode { .. } => self.refresh(candidates),
            Step::InstallDsh { version, .. } => {
                self.refresh(candidates);
                // 记下「谁装的、装到了哪」，回退下拉就靠它（见 task-5）。
                let program = self
                    .env
                    .dsh
                    .as_ref()
                    .map(|runtime| runtime.program.display().to_string())
                    .unwrap_or_default();
                self.settings
                    .record_install(&version, &program, stamp::now_unix());
                self.dirty = true;
            }
            Step::Backup { .. } | Step::Restore { .. } | Step::Import { .. } => {
                self.refresh(candidates);
            }
            Step::RepairDeps { .. } => {}
            Step::InstallPlugins { packages, .. } => {
                self.refresh(candidates);
                push_log(
                    &mut self.log,
                    "ok",
                    format!("已预装 {} 个推荐插件", packages.len()),
                );
            }
        }
        if self.pending.is_empty() && !self.busy() {
            self.rev = self.rev.wrapping_add(1);
        }
        // 队列跑完且环境就绪：不用用户再点，直接进主界面。
        // 只看 `autostart`（由 Reprobe 成功后置位）——「点开菜单」不该自己启动。
        if self.pending.is_empty()
            && self.running_is_idle()
            && self.env.ready(&self.settings)
            && self.autostart
        {
            self.autostart = false;
            return Effect::StartHost;
        }
        Effect::None
    }

    fn start_step(&mut self, step: Step) {
        self.rev = self.rev.wrapping_add(1);
        push_log(&mut self.log, "info", format!("⟳ {}", step.label()));
        let env = self.settings.install_env();
        let paths = self.paths.clone();

        let running = match step.clone() {
            Step::FetchShasums { version } => {
                let file = paths
                    .install_downloads()
                    .join(format!("SHASUMS256-{version}.txt"));
                match node::download(&node::shasums_url(&version), &file, &env) {
                    Ok(job) => Running::Job { job, step },
                    Err(err) => {
                        self.error = Some(format!("下载校验清单失败：{err}"));
                        Running::Idle
                    }
                }
            }
            Step::VerifyNode { version } => {
                let data = paths.data.clone();
                Running::Worker {
                    rx: spawn_worker(move || {
                        let downloads = data.join("downloads");
                        let archive = downloads.join(node::asset_name(&version));
                        let shasums = downloads.join(format!("SHASUMS256-{version}.txt"));
                        let text = std::fs::read_to_string(&shasums)
                            .map_err(|err| format!("读校验清单失败：{err}"))?;
                        let actual = node::sha256_file(&archive)
                            .map_err(|err| format!("算 SHA-256 失败：{err}"))?;
                        let asset = node::asset_name(&version);
                        if let Err(err) = node::verify_shasum(&text, &asset, &actual) {
                            // 校验不过就别留着这个包，免得下次被当成「已下载」。
                            let _ = std::fs::remove_file(&archive);
                            return Err(err);
                        }
                        let _ = std::fs::remove_file(&shasums);
                        Ok(format!("{asset} SHA-256 校验通过（{actual}）"))
                    }),
                    step,
                }
            }
            Step::InstallNode { version } => {
                // 下载走 curl（可取消、有进度），解压校验在后台线程。
                let archive = paths.install_downloads().join(node::asset_name(&version));
                match node::download(&node::tarball_url(&version), &archive, &env) {
                    Ok(job) => Running::Job { job, step },
                    Err(err) => {
                        self.error = Some(format!("下载 node 失败：{err}"));
                        Running::Idle
                    }
                }
            }
            Step::InstallDsh { version, scope } => match self.npm_command(&version, scope, &env) {
                Ok(job) => Running::Job { job, step },
                Err(err) => {
                    self.error = Some(err);
                    Running::Idle
                }
            },
            Step::InstallPlugins { profile, packages } => {
                // 所有东西都交给 dsh 自己做：它就是 pnpm 的转发层，而且会顺手把插件登记到
                // profile 的 `dsh.profile.bundles`（手写 package.json 很容易漏这一步）。
                let program = self.env.dsh.as_ref().map(|runtime| runtime.program.clone());
                let Some(program) = program else {
                    push_log(&mut self.log, "warn", "找不到 dsh，跳过推荐插件".to_owned());
                    return;
                };
                let mut args = vec![
                    "plugin".to_owned(),
                    "--profile".to_owned(),
                    profile,
                    "add".to_owned(),
                ];
                args.extend(packages);
                match Job::spawn(&program, &args, &env, None) {
                    Ok(job) => Running::Job { job, step },
                    Err(err) => {
                        push_log(
                            &mut self.log,
                            "warn",
                            format!("启动 dsh plugin 失败：{err}，跳过推荐插件"),
                        );
                        Running::Idle
                    }
                }
            }
            Step::RepairDeps { profile } => {
                let dir = paths.profiles_root().join(&profile);
                match Job::spawn(
                    &pnpm_program(),
                    &["install".to_owned(), "--prefer-offline".to_owned()],
                    &env,
                    Some(&dir),
                ) {
                    Ok(job) => Running::Job { job, step },
                    Err(err) => {
                        self.error = Some(format!("启动 pnpm 失败：{err}"));
                        Running::Idle
                    }
                }
            }
            Step::Backup { profile } => {
                let from = paths.profiles_root().join(&profile);
                let to = paths.backups();
                let at = stamp::now_unix();
                Running::Worker {
                    rx: spawn_worker(move || {
                        profiles::backup(&from, &to, at)
                            .map(|path| format!("已备份到 {}", path.display()))
                            .map_err(|err| format!("备份失败：{err}"))
                    }),
                    step,
                }
            }
            Step::Restore { archive, profile } => {
                let target = paths.profiles_root().join(&profile);
                let auto_snapshot_dir = paths.backups();
                Running::Worker {
                    rx: spawn_worker(move || {
                        // 还原前先给当前内容拍一份快照，手滑也能回来。
                        if target.exists() {
                            let snapshot =
                                profiles::backup(&target, &auto_snapshot_dir, stamp::now_unix())
                                    .map_err(|err| format!("还原前快照失败：{err}"))?;
                            let _ = snapshot;
                        }
                        profiles::restore(&archive, &target)
                            .map(|()| format!("已还原「{profile}」"))
                            .map_err(|err| format!("还原失败：{err}"))
                    }),
                    step,
                }
            }
            Step::Import { archive, name } => {
                let root = paths.profiles_root();
                Running::Worker {
                    rx: spawn_worker(move || {
                        profiles::import(&archive, &root, &name)
                            .map(|path| {
                                format!("已导入 {}（用 pnpm 补依赖后才能启动）", path.display())
                            })
                            .map_err(|err| format!("导入失败：{err}"))
                    }),
                    step,
                }
            }
            Step::FetchVersions => {
                let npm = npm_program(&self.env);
                let env = env.clone();
                Running::Worker {
                    rx: spawn_worker(move || {
                        let result = dsh_setup::install::run_blocking(
                            &npm,
                            &[
                                "view".to_owned(),
                                "@deepseek-ai/dsh".to_owned(),
                                "versions".to_owned(),
                                "--json".to_owned(),
                            ],
                            &env,
                            None,
                            Duration::from_secs(30),
                        )
                        .map_err(|err| format!("{err}"))?;
                        if !result.success() {
                            return Err(result.failure_note());
                        }
                        Ok(result.stdout)
                    }),
                    step,
                }
            }
            Step::ExtractNode { version } => {
                let data = paths.data.clone();
                let archive = data.join("downloads").join(node::asset_name(&version));
                let prefix = node::version_prefix(&data, &version);
                Running::Worker {
                    rx: spawn_worker(move || {
                        let extracted = node::extract(&archive, &prefix)
                            .map_err(|err| format!("解压失败：{err}"))?;
                        if !extracted.success() {
                            return Err(extracted.failure_note());
                        }
                        let reported = node::verify_extracted(&prefix)
                            .map_err(|err| format!("自检失败：{err}"))?;
                        let _ = std::fs::remove_file(&archive);
                        Ok(format!("node {reported} 就绪：{}", prefix.display()))
                    }),
                    step,
                }
            }
            Step::Reprobe => {
                // 真正的重新探测在 `after_success` 里做（那里能拿到 candidates）。
                Running::Worker {
                    rx: spawn_worker(|| Ok(String::new())),
                    step,
                }
            }
        };

        if matches!(running, Running::Idle) {
            // 起不来就直接进进度页显示错误，别让界面停在按钮可点的状态。
            self.pending.clear();
            self.screen = Screen::Progress;
            self.rev = self.rev.wrapping_add(1);
            if let Some(err) = &self.error {
                push_log(&mut self.log, "err", err.clone());
            }
        }
        self.running = running;
    }

    fn npm_command(
        &self,
        version: &str,
        scope: InstallScope,
        env: &[(String, String)],
    ) -> Result<Job, String> {
        let npm = npm_program(&self.env);
        let mut args = vec![
            "install".to_owned(),
            "-g".to_owned(),
            format!("@deepseek-ai/dsh@{version}"),
            "--no-fund".to_owned(),
            "--no-audit".to_owned(),
        ];
        if scope == InstallScope::User {
            args.push("--prefix".to_owned());
            args.push(self.paths.user_prefix().display().to_string());
        }
        let mut env = env.to_vec();
        // 用我们自己装的 node 来跑 npm：把它的 bin 放到 PATH 最前面。
        if let Some(bin) = &self.env.node_bin {
            env.push((
                "PATH".to_owned(),
                envcheck::path_with(std::slice::from_ref(bin))
                    .to_string_lossy()
                    .into_owned(),
            ));
        }
        Job::spawn(&npm, &args, &env, None).map_err(|err| format!("启动 npm 失败：{err}"))
    }

    /// 处理界面命令。
    pub fn handle(&mut self, cmd: &Value, candidates: &[envcheck::Candidate]) -> Effect {
        let name = cmd.get("cmd").and_then(Value::as_str).unwrap_or_default();
        match name {
            "goto" => {
                if let Some(screen) = cmd
                    .get("screen")
                    .and_then(Value::as_str)
                    .and_then(Screen::from_key)
                {
                    self.screen = screen;
                    self.rev = self.rev.wrapping_add(1);
                }
            }
            "save" => self.apply_settings(cmd.get("settings")),
            "probe" => {
                // 刻意不看 `busy()`：拉版本列表（`npm view`）在慢网络上要十几秒，
                // 这期间用户点「重新检测」不该被当成无效操作默默吞掉。
                self.refresh(candidates);
                self.refresh_hosts();
                self.probes = self.probes.wrapping_add(1);
                // 环境没变时界面本来一个像素都不会动，点击就像没反应：给一句带次数的回执。
                let state = if self.env.ready(&self.settings) {
                    "环境就绪"
                } else {
                    "环境仍不完整"
                };
                let orphans = self.orphans().len();
                let hosts = if orphans > 0 {
                    format!("，{orphans} 个孤儿 dsh")
                } else {
                    String::new()
                };
                self.notice = Some(format!(
                    "已重新检测（第 {} 次）：{state}{hosts}",
                    self.probes
                ));
                self.rev = self.rev.wrapping_add(1);
            }
            "scan-hosts" => {
                self.refresh_hosts();
                self.notice = Some(format!(
                    "扫描到 {} 个 dsh 进程，其中 {} 个孤儿",
                    self.hosts.len(),
                    self.orphans().len()
                ));
                self.rev = self.rev.wrapping_add(1);
            }
            "reap-hosts" => {
                let before = self.orphans().len();
                let killed = crate::hostscan::reap(self.host_pid);
                self.refresh_hosts();
                self.notice = Some(if before == 0 {
                    "没有需要清理的孤儿 dsh".to_owned()
                } else {
                    format!("已清理 {killed} 个孤儿 dsh（原有 {before} 个）")
                });
                self.rev = self.rev.wrapping_add(1);
            }
            "install" => {
                self.request(cmd, candidates);
            }
            "update" | "install-version" => {
                if !self.busy() {
                    let version = cmd
                        .get("version")
                        .and_then(Value::as_str)
                        .filter(|v| !v.is_empty())
                        .unwrap_or(self.settings.target_dsh_version())
                        .to_owned();
                    self.settings.dsh_version = version.clone();
                    self.pending.clear();
                    self.pending.push_back(Step::InstallDsh {
                        version,
                        scope: self.settings.install_scope,
                    });
                    self.pending.push_back(Step::Reprobe);
                    self.screen = Screen::Progress;
                    self.error = None;
                    self.rev = self.rev.wrapping_add(1);
                }
            }
            "rollback" => {
                if !self.busy() {
                    let current = self
                        .env
                        .dsh
                        .as_ref()
                        .and_then(|d| d.version.as_ref())
                        .map(ToString::to_string);
                    if let Some(previous) = self.settings.previous_version(current.as_deref()) {
                        let version = previous.version.clone();
                        self.pending.clear();
                        self.pending.push_back(Step::InstallDsh {
                            version: version.clone(),
                            scope: self.settings.install_scope,
                        });
                        self.pending.push_back(Step::Reprobe);
                        self.screen = Screen::Progress;
                        self.error = None;
                        self.rev = self.rev.wrapping_add(1);
                    } else {
                        self.notice = Some("没有可回退的历史版本".to_owned());
                        self.rev = self.rev.wrapping_add(1);
                    }
                }
            }
            "backup" => {
                if !self.busy() {
                    let profile = self.settings.target_profile().to_owned();
                    self.pending.push_back(Step::Backup { profile });
                    self.screen = Screen::Progress;
                    self.rev = self.rev.wrapping_add(1);
                }
            }
            "restore" => {
                if !self.busy() {
                    let Some(archive) = self.selected_backup.clone() else {
                        self.notice = Some("先选一个备份".to_owned());
                        self.rev = self.rev.wrapping_add(1);
                        return Effect::None;
                    };
                    let profile = archive
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .and_then(|name| name.rsplit_once('-').map(|(p, _)| p.to_owned()))
                        .unwrap_or_else(|| self.settings.target_profile().to_owned());
                    self.pending.push_back(Step::Restore { archive, profile });
                    self.screen = Screen::Progress;
                    self.rev = self.rev.wrapping_add(1);
                }
            }
            "pick-backup" => {
                let dir = self.paths.backups();
                if let Some(path) = pick_archive(&dir) {
                    self.selected_backup = Some(path);
                    self.rev = self.rev.wrapping_add(1);
                }
            }
            // 界面上的备份列表是单选：选中只是改状态，不触发任何执行。
            "select-backup" => {
                if let Some(path) = cmd.get("path").and_then(Value::as_str) {
                    self.selected_backup = Some(PathBuf::from(path));
                    self.rev = self.rev.wrapping_add(1);
                }
            }
            "pick-import" => {
                if let Some(path) = pick_archive(&self.paths.backups()) {
                    self.pending_archive = Some(path);
                    self.rev = self.rev.wrapping_add(1);
                }
            }
            "import" => {
                if !self.busy() {
                    let Some(archive) = self.pending_archive.clone() else {
                        self.notice = Some("先选一个 .tar.gz 归档".to_owned());
                        self.rev = self.rev.wrapping_add(1);
                        return Effect::None;
                    };
                    let name = cmd
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_owned();
                    let name = if name.is_empty() {
                        archive
                            .file_stem()
                            .map(|s| s.to_string_lossy().trim_end_matches(".tar").to_owned())
                            .unwrap_or_else(|| "imported".to_owned())
                    } else {
                        name
                    };
                    if let Err(err) = profiles::validate_name(&name) {
                        self.notice = Some(err);
                        self.rev = self.rev.wrapping_add(1);
                        return Effect::None;
                    }
                    self.pending.push_back(Step::Import { archive, name });
                    self.screen = Screen::Progress;
                    self.rev = self.rev.wrapping_add(1);
                }
            }
            "repair-deps" => {
                if !self.busy() {
                    let profile = self.settings.target_profile().to_owned();
                    self.pending.push_back(Step::RepairDeps { profile });
                    self.screen = Screen::Progress;
                    self.rev = self.rev.wrapping_add(1);
                }
            }
            "cancel" => {
                if let Running::Job { job, .. } = &mut self.running {
                    job.cancel();
                }
                self.pending.clear();
                self.notice = Some("已取消".to_owned());
                self.rev = self.rev.wrapping_add(1);
            }
            "toggle-log" => {
                self.log_open = !self.log_open;
                self.rev = self.rev.wrapping_add(1);
            }
            "clear-log" => {
                self.log.clear();
                self.rev = self.rev.wrapping_add(1);
            }
            "quit" => return Effect::Quit,
            "open-logs" => return Effect::OpenLogs,
            "start" => {
                if self.env.ready(&self.settings) {
                    return Effect::StartHost;
                }
                self.notice = Some("环境还不完整，先完成安装".to_owned());
                self.rev = self.rev.wrapping_add(1);
            }
            // 页面上报的点击诊断（点空了 / 点到禁用按钮）：只在 stderr 留痕，不进用户看的「安装日志」。
            "miss" | "disabled" | "down" => {}
            other => {
                push_log(&mut self.log, "warn", format!("未知命令 {other:?}"));
                self.rev = self.rev.wrapping_add(1);
            }
        }
        Effect::None
    }

    fn apply_settings(&mut self, patch: Option<&Value>) {
        let Some(patch) = patch else { return };
        let Some(object) = patch.as_object() else {
            return;
        };
        let text = |value: Option<&Value>| {
            value
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_owned()
        };
        if object.contains_key("http")
            || object.contains_key("https")
            || object.contains_key("all")
            || object.contains_key("no_proxy")
        {
            self.settings.proxy.http = text(object.get("http"));
            self.settings.proxy.https = text(object.get("https"));
            self.settings.proxy.all = text(object.get("all"));
            self.settings.proxy.no_proxy = text(object.get("no_proxy"));
        }
        if object.contains_key("npm_registry") {
            self.settings.npm_registry = text(object.get("npm_registry"));
        }
        if object.contains_key("dsh_version") {
            self.settings.dsh_version = text(object.get("dsh_version"));
        }
        if object.contains_key("node_version") {
            self.settings.node_version = text(object.get("node_version"));
        }
        if let Some(scope) = object.get("install_scope").and_then(Value::as_str) {
            self.settings.install_scope = match scope {
                "global" => InstallScope::Global,
                "user" => InstallScope::User,
                _ => InstallScope::Auto,
            };
        }
        if object.contains_key("profile") {
            self.settings.profile = text(object.get("profile"));
        }
        if let Some(values) = object.get("plugins") {
            // 只认推荐清单里的名字：手改 settings.json 不该能往用户机器上装任意包。
            self.settings.plugins = Some(
                values
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .filter_map(Value::as_str)
                            .filter(|name| dsh_setup::plugins::is_recommended(name))
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
            );
        }
        if object.contains_key("extra_path") {
            self.settings.extra_path = text(object.get("extra_path"))
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect();
        }
        if object.contains_key("env") {
            self.settings.env = text(object.get("env"))
                .lines()
                .filter_map(|line| {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        return None;
                    }
                    let (key, value) = line.split_once('=')?;
                    let key = key.trim();
                    (!key.is_empty()).then(|| (key.to_owned(), value.trim().to_owned()))
                })
                .collect();
        }
        self.dirty = true;
        // 刻意不动 `rev`：设置是用户边打字边存的，整块重画会把输入焦点抢走。
    }

    /// 「开始安装」：正在拉版本 / 装东西时先存着，队列空了自动重放。
    fn request(&mut self, cmd: &Value, candidates: &[envcheck::Candidate]) {
        if self.busy() {
            self.notice = Some("已排队：当前步骤结束后立即开始安装".to_owned());
            self.queued = Some(cmd.clone());
            self.rev = self.rev.wrapping_add(1);
            return;
        }
        self.plan_install(candidates);
    }

    fn plan_install(&mut self, candidates: &[envcheck::Candidate]) {
        self.pending.clear();
        self.queued = None;
        self.error = None;
        self.notice = None;
        self.screen = Screen::Progress;
        if !self.env.node.is_ready() {
            let version = if self.settings.node_version.is_empty() {
                FALLBACK_NODE_VERSION.to_owned()
            } else {
                self.settings.node_version.clone()
            };
            // 记下来：装完后 `launch_spec` 要把这份 node 的 bin 目录塞进子进程 PATH，
            // 否则 `#!/usr/bin/env node` 的 dsh shim 在系统只有 node 18 的机器上起不来。
            self.settings.node_version = version.clone();
            self.dirty = true;
            self.pending.push_back(Step::FetchShasums {
                version: version.clone(),
            });
            self.pending.push_back(Step::InstallNode { version });
        }
        let version = self.settings.target_dsh_version().to_owned();
        self.pending.push_back(Step::InstallDsh {
            version,
            scope: self.settings.install_scope,
        });
        // 插件必须排在 dsh 之后：命令本身就是刚装好的 dsh 提供的。
        let packages = self.settings.selected_plugins();
        if !packages.is_empty() {
            self.pending.push_back(Step::InstallPlugins {
                profile: self.settings.target_profile().to_owned(),
                packages,
            });
        }
        self.pending.push_back(Step::Reprobe);
        self.refresh(candidates);
        self.rev = self.rev.wrapping_add(1);
    }

    /// 界面渲染用的快照。
    #[must_use]
    pub fn view(&self) -> Value {
        let (busy_label, busy) = match &self.running {
            Running::Job { step, .. } | Running::Worker { step, .. } => (step.label(), true),
            Running::Idle => (String::new(), !self.pending.is_empty()),
        };
        let node = match &self.env.node {
            NodeRuntime::Missing => json!({"state": "missing"}),
            NodeRuntime::TooOld { program, version } => json!({
                "state": "too_old",
                "version": version.to_string(),
                "program": program.display().to_string(),
            }),
            NodeRuntime::Ready { program, version } => json!({
                "state": "ready",
                "version": version.to_string(),
                "program": program.display().to_string(),
            }),
        };
        let dsh = match &self.env.dsh {
            None => json!({"state": "missing"}),
            Some(runtime) => json!({
                "state": if runtime.supported() { "ready" } else { "out_of_range" },
                "version": runtime.version.as_ref().map(ToString::to_string),
                "program": runtime.program.display().to_string(),
                "source": runtime.source.label(),
                "supported": runtime.supported(),
            }),
        };
        let backups: Vec<Value> = profiles::backups(&self.paths.backups())
            .into_iter()
            .map(|file| {
                json!({
                    "path": file.path.display().to_string(),
                    "profile": file.profile,
                    "stamp": file.stamp,
                    "bytes": file.bytes,
                })
            })
            .collect();
        let selected_plugins = self.settings.selected_plugins();
        json!({
            "rev": self.rev,
            "screen": self.screen.key(),
            "visible": self.visible,
            "banner": self.banner.as_ref().map(|(summary, detail)| json!({
                "summary": summary,
                "detail": detail,
            })),
            "error": self.error,
            "notice": self.notice,
            "busy": busy,
            "busy_label": busy_label,
            "log_open": self.log_open,
            "log": self.log.iter().map(|line| json!({"kind": line.kind, "text": line.text})).collect::<Vec<_>>(),
            "env": {
                "node": node,
                "dsh": dsh,
                "profiles": self.env.profiles.iter().map(|profile| json!({
                    "name": profile.name,
                    "web": profile.web_capable,
                    "deps": profile.has_deps,
                    "bundles": profile.bundles,
                })).collect::<Vec<_>>(),
                "web_profiles": self.env.web_profiles(),
                "ready": self.env.ready(&self.settings),
                "data_dir": self.paths.data.display().to_string(),
                "dsh_home": self.paths.dsh_home.display().to_string(),
                "installed_nodes": self.env.installed_nodes,
            },
            "settings": {
                "http": self.settings.proxy.http,
                "https": self.settings.proxy.https,
                "all": self.settings.proxy.all,
                "no_proxy": self.settings.proxy.no_proxy,
                "npm_registry": self.settings.npm_registry,
                "dsh_version": self.settings.target_dsh_version(),
                "node_version": self.settings.node_version,
                "install_scope": match self.settings.install_scope {
                    InstallScope::Auto => "auto",
                    InstallScope::Global => "global",
                    InstallScope::User => "user",
                },
                "profile": self.settings.target_profile(),
                "plugins": self.settings.selected_plugins(),
                "host_env": {
                    "extra_path": self.settings.extra_path.join("\n"),
                    "env": self.settings.env.iter()
                        .map(|(key, value)| format!("{key}={value}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                },
            },
            "versions": self.versions,
            "plugins": {
                "recommended": dsh_setup::plugins::RECOMMENDED.iter().map(|item| json!({
                    "name": item.name,
                    "title": item.title,
                    "blurb": item.blurb,
                    "selected": selected_plugins.iter().any(|name| name == item.name),
                })).collect::<Vec<_>>(),
                "selected": selected_plugins,
            },
            "hosts": {
                "shell": std::process::id(),
                "current": self.hosts.iter()
                    .find(|proc| Some(proc.pid) == self.host_pid)
                    .map(|proc| json!({
                        "pid": proc.pid,
                        "ppid": proc.ppid,
                        "uptime": proc.uptime,
                        "command": proc.command,
                        "url": self.host_url,
                        "ready": self.host_ready,
                    })),
                "expected_pid": self.host_pid,
                "orphans": self.orphans().into_iter().map(|proc| json!({
                    "pid": proc.pid,
                    "ppid": proc.ppid,
                    "uptime": proc.uptime,
                    "command": proc.command,
                })).collect::<Vec<_>>(),
                "total": self.hosts.len(),
                // 进程扫描靠 `ps`，Windows 没有；别让界面把「扫不了」显示成「没有孤儿」。
                "scan_supported": cfg!(unix),
            },
            "history": self.settings.history.iter().map(|entry| json!({
                "version": entry.version,
                "program": entry.program,
            })).collect::<Vec<_>>(),
            "backups": backups,
            "selected_backup": self.selected_backup.as_ref().map(|p| p.display().to_string()),
            "pending_archive": self.pending_archive.as_ref().map(|p| p.display().to_string()),
            "paths": {
                "backups": self.paths.backups().display().to_string(),
                "user_prefix": self.paths.user_prefix().display().to_string(),
            },
        })
    }

    /// 解析 `npm view` 的结果，填充版本下拉。
    pub fn ingest_versions(&mut self, raw: &str) {
        let Ok(value) = serde_json::from_str::<Value>(raw) else {
            return;
        };
        let mut versions: Vec<String> = value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        // 只保留 0.2.x 之后能当宿主的（< 0.3 且 >= MIN_HOST_VERSION）。
        let min = envcheck::min_host();
        let max = envcheck::max_host();
        versions.retain(|text| {
            Version::parse(text).is_some_and(|version| version.in_range(&min, &max))
        });
        versions.sort_by(|a, b| match (Version::parse(a), Version::parse(b)) {
            (Some(a), Some(b)) => b.cmp(&a),
            _ => b.cmp(a),
        });
        versions.dedup();
        if versions.is_empty() {
            versions.push(settings::DEFAULT_DSH_VERSION.to_owned());
        }
        self.versions = versions;
        self.rev = self.rev.wrapping_add(1);
    }
}

/// 哪些步骤的返回值是给人看的日志（而不是要解析的数据）。
fn step_note_wanted(step: &Step) -> bool {
    !matches!(step, Step::FetchVersions | Step::Reprobe)
}

fn push_log(log: &mut Vec<LogLine>, kind: &'static str, text: String) {
    // 页面上的「安装日志」跟一份到 stderr：用户只能截图，日志文件里能直接查。
    eprintln!("dsh-desktop: 引导页日志 [{kind}] {text}");
    log.push(LogLine { kind, text });
    // 安装日志几百行足够定位问题；不设上限会让 view() 每次 tick 都序列化一大坨。
    if log.len() > 2000 {
        log.drain(..500);
    }
}

fn describe_failure(stderr_tail: &str, code: Option<i32>) -> String {
    let last = stderr_tail
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_owned());
    match last {
        Some(line) => format!("命令失败（退出码 {code:?}）：{line}"),
        None => format!("命令失败（退出码 {code:?}）"),
    }
}

fn looks_like_permission_problem(err: &str) -> bool {
    let lower = err.to_lowercase();
    lower.contains("eacces")
        || lower.contains("permission denied")
        || lower.contains("permission_error")
        || err.contains("EACCES")
}

/// npm 在哪：优先我们装的 node 旁边的 npm，其次 PATH。
fn npm_program(env: &EnvReport) -> PathBuf {
    if let Some(bin) = &env.node_bin {
        // Windows 上要拿 `npm.cmd`：无扩展名的 `npm` 是 sh 脚本，spawn 直接 error 193。
        if let Some(candidate) = envcheck::program_in(bin, "npm") {
            return candidate;
        }
    }
    envcheck::find_in_path("npm").unwrap_or_else(|| PathBuf::from("npm"))
}

/// 同 `npm_program`，只是找 pnpm（Windows 上是 `pnpm.cmd`，直接 spawn `pnpm` 会 error 193）。
fn pnpm_program() -> PathBuf {
    envcheck::resolve_program("pnpm")
}

fn spawn_worker(
    job: impl FnOnce() -> Result<String, String> + Send + 'static,
) -> Receiver<Result<String, String>> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = tx.send(job());
    });
    rx
}

fn pick_archive(start: &Path) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new().add_filter("tar.gz", &["gz", "tgz", "tar"]);
    if start.is_dir() {
        dialog = dialog.set_directory(start);
    }
    dialog.pick_file()
}

/// `npm prefix -g` 下的 `bin/dsh`：全新机器上「优先全局安装」装完就落在这里。
///
/// 现在有了这条候选，全局装成功也能被重探测认出来（之前只有用户前缀 + PATH，
/// npm 全局前缀不在 PATH 时会一直卡在「环境不全」）。
/// 只在壳启动时算一次——起一次 npm 要几百毫秒，不能每帧跑。
///
/// **不检查文件是否存在**：前缀是稳定的，dsh 是后来才装进去的；
/// 调用方（`App::candidates`）每次探测时再按 `is_file()` 过滤。
#[must_use]
pub fn npm_global_dsh(npm: &Path, env: &[(String, String)]) -> Option<PathBuf> {
    let result = run_blocking(
        npm,
        &["prefix".to_owned(), "-g".to_owned()],
        env,
        None,
        Duration::from_secs(10),
    )
    .ok()?;
    if !result.success() {
        return None;
    }
    let prefix = result.stdout.trim();
    if prefix.is_empty() {
        return None;
    }
    // npm 的前缀布局分平台：macOS / Linux 是 `<prefix>/bin/dsh`，Windows 直接是 `<prefix>/dsh.cmd`。
    #[cfg(windows)]
    let program = {
        let dir = Path::new(prefix);
        ["dsh.cmd", "dsh.exe", "dsh.ps1"]
            .iter()
            .map(|name| dir.join(name))
            .find(|candidate| candidate.is_file())
            .unwrap_or_else(|| dir.join("dsh.cmd"))
    };
    #[cfg(not(windows))]
    let program = Path::new(prefix).join("bin").join("dsh");
    Some(program)
}

/// 探测环境。`candidates` 是 dsh 的候选路径（由壳按优先级给出）。
#[must_use]
pub fn detect(paths: &Paths, candidates: &[envcheck::Candidate], settings: &Settings) -> EnvReport {
    let installed_nodes = installed_node_versions(paths);
    let mut extra: Vec<PathBuf> = installed_nodes
        .iter()
        .map(|version| node::bin_dir(&paths.data, version))
        .filter(|dir| dir.is_dir())
        .collect();
    // 用户点过「用哪个 node」就把那个放到最前。
    if !settings.node_version.is_empty() {
        let chosen = node::bin_dir(&paths.data, &settings.node_version);
        if chosen.is_dir() {
            extra.retain(|dir| *dir != chosen);
            extra.insert(0, chosen);
        }
    }

    let node = envcheck::detect_node(&extra);
    let node_bin = match &node {
        NodeRuntime::Ready { program, .. } | NodeRuntime::TooOld { program, .. } => {
            program.parent().map(Path::to_path_buf)
        }
        NodeRuntime::Missing => None,
    };

    let mut dsh_search = extra.clone();
    if let Some(bin) = &node_bin {
        dsh_search.push(bin.clone());
    }
    // 引导页自己装的那份永远参与探测：调用方传不传候选都算得到。
    let user_dsh = paths.user_dsh();
    let mut candidates: Vec<envcheck::Candidate> = candidates.to_vec();
    if !candidates.iter().any(|c| c.program == user_dsh) {
        candidates.push(envcheck::Candidate::new(
            user_dsh,
            envcheck::Source::UserPrefix,
        ));
    }
    // 向导自己装的 node 的全局前缀：`npm i -g` 落到 app 的 node 下（一定可写，
    // 不会 EACCES 兜底），而这条路径不在 PATH 上——不扫码就得不出 "已安装"。
    for bin in &extra {
        let program = bin.join("dsh");
        if program.is_file() && !candidates.iter().any(|c| c.program == program) {
            candidates.push(envcheck::Candidate::new(
                program,
                envcheck::Source::NodePrefix,
            ));
        }
    }
    let dsh = envcheck::detect_dsh(&candidates, &dsh_search);

    EnvReport {
        node,
        node_bin,
        dsh,
        profiles: profiles::list(&paths.profiles_root()),
        installed_nodes,
    }
}

/// 壳自己装的每个 node 前缀里已经存在的那份 `dsh`（按版本新到旧）。
///
/// 这些路径不在 PATH 上：全新机器上「优先全局安装」就落在这里，启动时也必须能找到。
#[must_use]
pub fn node_prefix_candidates(paths: &Paths) -> Vec<envcheck::Candidate> {
    let mut out = Vec::new();
    for version in installed_node_versions(paths) {
        let program = node::bin_dir(&paths.data, &version).join("dsh");
        if program.is_file() {
            out.push(envcheck::Candidate::new(
                program,
                envcheck::Source::NodePrefix,
            ));
        }
    }
    out
}

fn installed_node_versions(paths: &Paths) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(node::install_root(&paths.data)) else {
        return Vec::new();
    };
    let mut versions: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_prefix('v').map(str::to_owned)
        })
        .collect();
    versions.sort_by(|a, b| match (Version::parse(a), Version::parse(b)) {
        (Some(a), Some(b)) => b.cmp(&a),
        _ => b.cmp(a),
    });
    versions
}

#[cfg(all(test, unix))]
mod tests {
    // 整套测试用 `#!/bin/sh` 假脚本驱动 node/npm/dsh，Windows 上既没有 sh 可跑，
    // `std::os::unix` 在那边也根本不存在 —— 与 supervisor/tests/lifecycle.rs 一样整体 unix-only。
    use super::*;

    fn scratch(name: &str) -> (Paths, PathBuf) {
        let root = std::env::temp_dir().join(format!("dsh-onboard-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("建临时目录");
        let paths = Paths {
            data: root.join("data"),
            state: root.join("state"),
            dsh_home: root.join("dsh"),
        };
        (paths, root)
    }

    fn fresh(paths: &Paths) -> Onboard {
        Onboard::new(paths.clone(), Settings::default(), &[])
    }

    /// 造一个「自己装的 node」+ 假的 npm，避免测试真的联网装包。
    /// 返回 npm 的调用记录文件。
    fn fake_node(data: &Path, npm_body: &str) -> PathBuf {
        let bin = node::bin_dir(data, "24.13.0");
        std::fs::create_dir_all(&bin).expect("建假 node 目录");
        // 写临时名 → 加执行位 → 改名：并行单测里别的线程一 fork，写 fd 就会被继承，
        // 直接 exec 刚写完的同名脚本会撞 ETXTBSY（ExecutableFileBusy）。
        let node = bin.join("node");
        let npm = bin.join("npm");
        for (path, body) in [(&node, "#!/bin/sh\necho v24.13.0\n"), (&npm, npm_body)] {
            use std::os::unix::fs::PermissionsExt;
            let staging = path.with_extension("staging");
            std::fs::write(&staging, body).expect("写假工具");
            std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))
                .expect("加执行位");
            std::fs::rename(&staging, path).expect("改名到正式名字");
        }
        data.join("npm-calls.log")
    }

    /// 写一个可执行脚本（临时名 → 加执行位 → 改名，避开 ETXTBSY）。
    fn write_script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        let staging = path.with_extension("staging");
        std::fs::write(&staging, body).expect("写脚本");
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))
            .expect("加执行位");
        std::fs::rename(&staging, path).expect("改名到正式名字");
    }

    fn drive(onboard: &mut Onboard) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while onboard.busy() && std::time::Instant::now() < deadline {
            onboard.tick(&[]);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!onboard.busy(), "队列没有跑完：{:?}", onboard.error);
    }

    fn log_text(onboard: &Onboard) -> String {
        onboard.view()["log"]
            .as_array()
            .expect("日志是数组")
            .iter()
            .filter_map(|line| line["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn installing_into_the_user_prefix_records_history() {
        let (paths, root) = scratch("install-user");
        // 假 npm 把每次调用的参数追加到记录文件里，脚本里直接写死路径最省事。
        let body = "#!/bin/sh\necho \"$*\" >> \"@CALLS@\"\necho 'added 42 packages'\n";
        let calls = paths.data.join("npm-calls.log");
        let body = body.replace("@CALLS@", &calls.display().to_string());
        fake_node(&paths.data, &body);

        let settings = Settings {
            install_scope: InstallScope::User,
            dsh_version: "0.2.2-alpha.3".to_owned(),
            ..Settings::default()
        };
        let mut onboard = Onboard::new(paths.clone(), settings, &[]);
        onboard.refresh(&[]);
        assert!(onboard.env.node.is_ready(), "假 node 应该被认出来");

        onboard.handle(&json!({"kind": "onboard", "cmd": "install"}), &[]);
        drive(&mut onboard);

        assert!(onboard.error.is_none(), "不该失败：{:?}", onboard.error);
        let calls = std::fs::read_to_string(&calls).expect("npm 被调用了");
        assert!(
            calls.contains("install -g @deepseek-ai/dsh@0.2.2-alpha.3"),
            "{calls}"
        );
        assert!(
            calls.contains(&format!("--prefix {}", paths.user_prefix().display())),
            "用户目录安装必须带 --prefix：{calls}"
        );
        // 装完要记进历史，否则「回退」下拉永远是空的。
        assert_eq!(
            onboard
                .settings
                .history
                .first()
                .map(|entry| entry.version.as_str()),
            Some("0.2.2-alpha.3")
        );
        assert!(onboard.take_dirty());
        assert!(log_text(&onboard).contains("added 42 packages"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn finishing_an_install_starts_the_host_by_itself() {
        let (paths, root) = scratch("install-autostart");
        // 假 npm：装上之后真的写出一份能跑 `--version` 的 dsh，否则复检不会就绪。
        let prefix = paths.user_prefix();
        let body = format!(
            "#!/bin/sh\nmkdir -p '{prefix}/bin'\nprintf '#!/bin/sh\\necho 0.2.2-alpha.3\\n' > '{prefix}/bin/dsh'\nchmod +x '{prefix}/bin/dsh'\necho 'added 42 packages'\n",
            prefix = prefix.display()
        );
        fake_node(&paths.data, &body);
        // 就绪还要求有一个能当宿主的 profile（带 Web App 包）。
        let web = paths.profiles_root().join("web");
        std::fs::create_dir_all(&web).expect("建 profile 目录");
        std::fs::write(
            web.join("package.json"),
            r#"{"dsh":{"profile":{"bundles":["@deepseek-ai/dsh-web-app"]}}}"#,
        )
        .expect("写 profile");

        let settings = Settings {
            install_scope: InstallScope::User,
            dsh_version: "0.2.2-alpha.3".to_owned(),
            ..Settings::default()
        };
        let mut onboard = Onboard::new(paths.clone(), settings, &[]);
        onboard.refresh(&[]);
        onboard.handle(&json!({"kind": "onboard", "cmd": "install"}), &[]);

        let mut effects = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while onboard.busy() && std::time::Instant::now() < deadline {
            effects.push(onboard.tick(&[]));
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(onboard.error.is_none(), "不该失败：{:?}", onboard.error);
        assert!(
            effects.contains(&Effect::StartHost),
            "装完应当自动启动宿主，实际效果 {effects:?}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn permission_error_falls_back_to_the_user_prefix() {
        let (paths, root) = scratch("install-fallback");
        let calls = paths.data.join("npm-calls.log");
        // 第一次跑：模拟系统目录没权限；第二次（带 --prefix）成功。
        let body = "#!/bin/sh\necho \"$*\" >> \"@CALLS@\"\ncase \"$*\" in\n  *--prefix*) echo 'added 42 packages' ; exit 0 ;;\nesac\necho 'npm ERR! code EACCES' >&2\necho 'npm ERR! EACCES: permission denied, mkdir /usr/lib/node_modules/@deepseek-ai' >&2\nexit 1\n"
            .replace("@CALLS@", &calls.display().to_string());
        fake_node(&paths.data, &body);

        let settings = Settings {
            install_scope: InstallScope::Auto,
            ..Settings::default()
        };
        let mut onboard = Onboard::new(paths.clone(), settings, &[]);
        onboard.refresh(&[]);
        onboard.handle(&json!({"kind": "onboard", "cmd": "install"}), &[]);
        drive(&mut onboard);

        assert!(onboard.error.is_none(), "不该失败：{:?}", onboard.error);
        let calls = std::fs::read_to_string(&calls).expect("npm 被调用了");
        let runs: Vec<&str> = calls.lines().collect();
        assert_eq!(runs.len(), 2, "应当先试全局、再退用户目录：{calls}");
        assert!(!runs[0].contains("--prefix"), "{calls}");
        assert!(runs[1].contains("--prefix"), "{calls}");
        assert!(log_text(&onboard).contains("改装到用户目录"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn view_serialises_the_first_step() {
        let (paths, root) = scratch("view");
        let onboard = fresh(&paths);
        let view = onboard.view();
        assert_eq!(view["screen"], "env");
        assert_eq!(view["visible"], false);
        assert_eq!(
            view["paths"]["user_prefix"],
            paths.user_prefix().display().to_string()
        );
        assert_eq!(view["settings"]["profile"], settings::DEFAULT_PROFILE);
        assert_eq!(
            view["settings"]["dsh_version"],
            settings::DEFAULT_DSH_VERSION
        );
        assert_eq!(view["log"].as_array().map(Vec::len), Some(0));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn failure_diagnostic_lands_in_the_log_panel() {
        let (paths, root) = scratch("fail");
        let mut onboard = fresh(&paths);
        let mut host_log = VecDeque::new();
        host_log.push_back("booting".to_owned());
        host_log.push_back("cannot bind".to_owned());
        onboard.note_failure(
            "无法启动本地服务",
            "执行 /nope/dsh 失败：No such file",
            "/nope/dsh --profile web",
            &host_log,
        );
        let view = onboard.view();
        let lines: Vec<String> = view["log"]
            .as_array()
            .expect("日志是数组")
            .iter()
            .map(|line| line["text"].as_str().unwrap_or_default().to_owned())
            .collect();
        assert!(lines.iter().any(|line| line.contains("无法启动本地服务")));
        // 原来失败页里可复制的那段诊断，现在必须在日志里。
        assert!(
            lines
                .iter()
                .any(|line| line.contains("/nope/dsh --profile web"))
        );
        assert!(lines.iter().any(|line| line == "cannot bind"));
        assert_eq!(view["log_open"], true);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn saving_settings_keeps_the_revision_stable() {
        let (paths, root) = scratch("save");
        let mut onboard = fresh(&paths);
        let before = onboard.rev();
        let request = json!({
            "kind": "onboard",
            "cmd": "save",
            "settings": {
                "http": "http://127.0.0.1:7890",
                "npm_registry": "https://registry.npmmirror.com",
                "install_scope": "user",
                "profile": "open-design",
                "node_version": "24.13.0",
            },
        });
        assert!(matches!(onboard.handle(&request, &[]), Effect::None));
        // 设置是边打字边存的，rev 变化会让整块面板重画、把输入焦点抢走。
        assert_eq!(onboard.rev(), before);
        assert!(onboard.take_dirty());
        assert_eq!(onboard.settings.proxy.http, "http://127.0.0.1:7890");
        assert_eq!(
            onboard.settings.npm_registry,
            "https://registry.npmmirror.com"
        );
        assert_eq!(onboard.settings.install_scope, InstallScope::User);
        assert_eq!(onboard.settings.profile, "open-design");
        assert_eq!(onboard.settings.node_version, "24.13.0");
        // 脏标记只能被吃一次。
        assert!(!onboard.take_dirty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn goto_moves_between_screens_and_ignores_unknown_names() {
        let (paths, root) = scratch("goto");
        let mut onboard = fresh(&paths);
        let goto = |screen: &str| json!({"kind": "onboard", "cmd": "goto", "screen": screen});
        onboard.handle(&goto("version"), &[]);
        assert_eq!(onboard.view()["screen"], "version");
        onboard.handle(&goto("nope"), &[]);
        assert_eq!(onboard.view()["screen"], "version");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn selecting_a_backup_only_marks_it() {
        let (paths, root) = scratch("backup");
        let mut onboard = fresh(&paths);
        let archive = paths.backups().join("web-20260101T000000Z.tar.gz");
        let request = json!({
            "kind": "onboard",
            "cmd": "select-backup",
            "path": archive.display().to_string(),
        });
        assert!(matches!(onboard.handle(&request, &[]), Effect::None));
        assert_eq!(
            onboard.view()["selected_backup"],
            archive.display().to_string()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn version_picker_falls_back_to_the_supported_default() {
        let (paths, root) = scratch("versions");
        let mut onboard = fresh(&paths);
        // 上游已经删掉的版本号、以及落在区间外的版本，都不能出现在下拉里。
        onboard.ingest_versions(
            r#"["0.1.1-rc.2","0.2.1-rc.5","0.3.0","0.2.2-alpha.3","0.2.2-alpha.2"]"#,
        );
        let view = onboard.view();
        let versions: Vec<String> = view["versions"]
            .as_array()
            .expect("版本是数组")
            .iter()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect();
        assert_eq!(versions, vec!["0.2.2-alpha.3", "0.2.2-alpha.2"]);
        // 解析不出来的时候至少给一个能装的。
        onboard.ingest_versions("not json at all");
        let fallback = onboard.view()["versions"]
            .as_array()
            .and_then(|items| items.first().cloned())
            .and_then(|value| value.as_str().map(str::to_owned));
        assert_eq!(fallback.as_deref(), Some(settings::DEFAULT_DSH_VERSION));
        let _ = std::fs::remove_dir_all(root);
    }

    /// 下载下来的 node 包必须对得上 SHASUMS256.txt：对不上就删包、报错、不进解压。
    #[test]
    fn a_bad_shasum_stops_the_node_install() {
        let (paths, root) = scratch("node-shasum-bad");
        let downloads = paths.data.join("downloads");
        std::fs::create_dir_all(&downloads).expect("建下载目录");
        let version = "24.13.0";
        let asset = node::asset_name(version);
        let archive = downloads.join(&asset);
        std::fs::write(&archive, b"pretend tarball").expect("写假包");
        std::fs::write(
            downloads.join(format!("SHASUMS256-{version}.txt")),
            format!("{}  {asset}\n", "0".repeat(64)),
        )
        .expect("写校验清单");

        let mut onboard = fresh(&paths);
        onboard.start_step(Step::VerifyNode {
            version: version.to_owned(),
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while onboard.busy() && std::time::Instant::now() < deadline {
            onboard.tick(&[]);
            std::thread::sleep(Duration::from_millis(10));
        }
        let error = onboard.error.clone().unwrap_or_default();
        assert!(error.contains("校验失败"), "错误信息该点名校验：{error}");
        assert!(!archive.exists(), "校验不过的包必须删掉");
        assert!(
            !onboard
                .pending
                .iter()
                .any(|s| matches!(s, Step::ExtractNode { .. })),
            "校验不过不该继续解压"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// 从菜单打开引导页不该自己重启宿主：用户是来管理的，不是来看它闪一下的。
    #[test]
    fn opening_the_wizard_does_not_restart_the_host() {
        let (paths, root) = scratch("open-no-autostart");
        fake_node(&paths.data, "#!/bin/sh\necho 'added 1 package'\n");
        let web = paths.profiles_root().join("web");
        std::fs::create_dir_all(&web).expect("建 profile 目录");
        std::fs::write(
            web.join("package.json"),
            r#"{"dsh":{"profile":{"bundles":["@deepseek-ai/dsh-web-app"]}}}"#,
        )
        .expect("写 profile");
        let bin = root.join("path-bin");
        std::fs::create_dir_all(&bin).expect("建 bin");
        write_script(&bin.join("dsh"), "#!/bin/sh\necho 0.2.2-alpha.3\n");
        let candidates = vec![envcheck::Candidate::new(
            bin.join("dsh"),
            envcheck::Source::Path,
        )];

        let mut onboard = Onboard::new(paths.clone(), Settings::default(), &candidates);
        onboard.open(&candidates, None);
        assert_eq!(onboard.screen, Screen::Done, "环境就绪时停在概览页");
        for _ in 0..5 {
            assert_eq!(onboard.tick(&candidates), Effect::None, "不该自动启动");
        }
        let _ = std::fs::remove_dir_all(root);
    }

    /// 对得上就继续解压并自检（否则校验就是把安装彻底堵死）。
    #[test]
    fn a_good_shasum_lets_the_install_continue() {
        let (paths, root) = scratch("node-shasum-good");
        let downloads = paths.data.join("downloads");
        std::fs::create_dir_all(&downloads).expect("建下载目录");
        let version = "24.13.0";
        let asset = node::asset_name(version);
        let archive = downloads.join(&asset);

        // 造一个真的 tar.gz（里面有 bin/node），让解压 + 自检这两步也能跑通。
        let staging = root.join("staging");
        std::fs::create_dir_all(staging.join("bin")).expect("建打包目录");
        write_script(
            &staging.join("bin").join("node"),
            "#!/bin/sh\necho v24.13.0\n",
        );
        let packed = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&staging)
            .arg(".")
            .status()
            .expect("打包");
        assert!(packed.success(), "tar 打包该成功");

        let digest = node::sha256_file(&archive).expect("算摘要");
        std::fs::write(
            downloads.join(format!("SHASUMS256-{version}.txt")),
            format!("{digest}  {asset}\n"),
        )
        .expect("写校验清单");

        let mut onboard = fresh(&paths);
        onboard.start_step(Step::VerifyNode {
            version: version.to_owned(),
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while onboard.busy() && std::time::Instant::now() < deadline {
            onboard.tick(&[]);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(onboard.error.is_none(), "不该失败：{:?}", onboard.error);
        let node_bin = node::node_program(&node::version_prefix(&paths.data, version));
        assert!(node_bin.exists(), "校验通过后该解压出 node");
        assert!(
            matches!(onboard.env.node, NodeRuntime::Ready { .. }),
            "解压后该被认成可用的 node：{:?}",
            onboard.env.node
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// PATH 上已经有一份能用的 dsh 时，启动就该直接进主界面，而不是弹引导页。
    #[test]
    fn a_dsh_on_path_makes_the_environment_ready() {
        let (paths, root) = scratch("path-dsh");
        fake_node(&paths.data, "#!/bin/sh\necho 'added 1 package'\n");
        let web = paths.profiles_root().join("web");
        std::fs::create_dir_all(&web).expect("建 profile 目录");
        std::fs::write(
            web.join("package.json"),
            r#"{"dsh":{"profile":{"bundles":["@deepseek-ai/dsh-web-app"]}}}"#,
        )
        .expect("写 profile");

        let bin = root.join("path-bin");
        std::fs::create_dir_all(&bin).expect("建 bin");
        let dsh = bin.join("dsh");
        write_script(&dsh, "#!/bin/sh\necho 0.2.2-alpha.3\n");
        let candidates = vec![envcheck::Candidate::new(dsh, envcheck::Source::Path)];

        // 用 with 版本把「用户目录装没装」和「PATH 上有没有」分开看。
        let onboard = Onboard::new(paths.clone(), Settings::default(), &candidates);
        assert!(
            onboard.env.dsh.is_some(),
            "候选里的 dsh 该被探测到（{:?}）",
            onboard.env.dsh
        );
        assert!(onboard.env_ready(), "node + 合规 dsh + web profile = 就绪");
        let _ = std::fs::remove_dir_all(root);
    }

    /// 「优先全局安装」落在 npm 全局前缀时，重探测必须认得出那份 dsh。
    #[test]
    fn a_globally_installed_dsh_is_detected() {
        let (paths, root) = scratch("npm-global");
        fake_node(&paths.data, "#!/bin/sh\necho 'added 1 package'\n");
        let web = paths.profiles_root().join("web");
        std::fs::create_dir_all(&web).expect("建 profile 目录");
        std::fs::write(
            web.join("package.json"),
            r#"{"dsh":{"profile":{"bundles":["@deepseek-ai/dsh-web-app"]}}}"#,
        )
        .expect("写 profile");

        // 假 npm 的 `prefix -g` 报出一个装着 dsh 的前缀（模拟 nvm/fnm 可写的全局目录）。
        let prefix = root.join("global-prefix");
        std::fs::create_dir_all(prefix.join("bin")).expect("建全局 bin");
        write_script(
            &prefix.join("bin").join("dsh"),
            "#!/bin/sh\necho 0.2.2-alpha.3\n",
        );
        let npm = root.join("fake-npm");
        write_script(
            &npm,
            &format!(
                "#!/bin/sh\n[ \"$1\" = prefix ] && echo '{}' && exit 0\necho 'added 1 package'\n",
                prefix.display()
            ),
        );

        let found = npm_global_dsh(&npm, &[]).expect("该找到全局前缀下的 dsh");
        assert_eq!(found, prefix.join("bin").join("dsh"));

        let candidates = vec![envcheck::Candidate::new(found, envcheck::Source::NpmGlobal)];
        let onboard = Onboard::new(paths.clone(), Settings::default(), &candidates);
        assert!(onboard.env_ready(), "全局装好的 dsh 该让环境就绪");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_dsh_inside_the_app_node_prefix_is_detected() {
        // 全新机器上「优先全局安装」会落进 app 自己装的 node 前缀：那条路一定可写，
        // 不会触发用户目录兜底，而且不在 PATH 上——不扫这个目录就会永远「未安装」。
        let (paths, root) = scratch("node-prefix-dsh");
        fake_node(&paths.data, "#!/bin/sh\nexit 0\n");
        write_script(
            &node::bin_dir(&paths.data, "24.13.0").join("dsh"),
            "#!/bin/sh\necho 0.2.2-alpha.3\n",
        );
        let web = paths.profiles_root().join("web");
        std::fs::create_dir_all(&web).expect("建 profile 目录");
        std::fs::write(
            web.join("package.json"),
            r#"{"dsh":{"profile":{"bundles":["@deepseek-ai/dsh-web-app"]}}}"#,
        )
        .expect("写 profile");

        let settings = Settings::default();
        let report = detect(&paths, &[], &settings);
        let dsh = report.dsh.as_ref().expect("node 前缀里的 dsh 要被扫出来");
        assert_eq!(dsh.source, envcheck::Source::NodePrefix);
        assert_eq!(
            dsh.version.as_ref().map(ToString::to_string).as_deref(),
            Some("0.2.2-alpha.3")
        );
        assert!(report.ready(&settings), "装完该判就绪，而不是死循环重试");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn planning_a_node_install_remembers_the_chosen_node() {
        let (paths, root) = scratch("plan-node");
        let mut onboard = Onboard::new(paths.clone(), Settings::default(), &[]);
        onboard.refresh(&[]);
        // 本机 PATH 上就有 node，这里直接擦成「没有」，模拟全新机器。
        onboard.env.node = NodeRuntime::Missing;
        onboard.env.node_bin = None;
        onboard.handle(&json!({"kind": "onboard", "cmd": "install"}), &[]);
        // 记下来才能让 `launch_spec` 把这份 node 的 bin 目录塞进宿主 PATH。
        assert_eq!(onboard.settings.node_version, FALLBACK_NODE_VERSION);
        assert!(onboard.take_dirty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_fresh_machine_without_profiles_is_ready() {
        // 全新机器上一个 profile 也没有：dsh 首次带 `--profile web` 起来会按模板建，
        // 所以空目录不该把用户卡在引导页里。
        let (paths, root) = scratch("fresh-profiles");
        fake_node(&paths.data, "#!/bin/sh\nexit 0\n");
        std::fs::create_dir_all(paths.user_dsh().parent().unwrap()).expect("建用户前缀 bin");
        write_script(&paths.user_dsh(), "#!/bin/sh\necho 0.2.2-alpha.3\n");
        let web = paths.profiles_root().join("web");
        std::fs::create_dir_all(web.parent().unwrap()).expect("建 profiles 目录");
        let settings = Settings::default();
        let report = detect(&paths, &[], &settings);
        assert!(
            report.profiles.is_empty(),
            "这里模拟的就是一个 profile 都没"
        );
        assert!(report.ready(&settings), "空 profile 目录也该判就绪");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn clicking_install_while_versions_load_still_starts() {
        // 刚打开向导就在拉版本列表；此时点「开始安装」以前会被 busy 默默丢掉，按钮像坏的。
        let (paths, root) = scratch("queued-install");
        let prefix = paths.user_prefix();
        let body = format!(
            "#!/bin/sh\nmkdir -p '{prefix}/bin'\nprintf '#!/bin/sh\\necho 0.2.2-alpha.3\\n' > '{prefix}/bin/dsh'\nchmod +x '{prefix}/bin/dsh'\necho 'added 42 packages'\n",
            prefix = prefix.display()
        );
        fake_node(&paths.data, &body);
        let web = paths.profiles_root().join("web");
        std::fs::create_dir_all(&web).expect("建 profile 目录");
        std::fs::write(
            web.join("package.json"),
            r#"{"dsh":{"profile":{"bundles":["@deepseek-ai/dsh-web-app"]}}}"#,
        )
        .expect("写 profile");

        let settings = Settings {
            install_scope: InstallScope::User,
            ..Settings::default()
        };
        let mut onboard = Onboard::new(paths.clone(), settings, &[]);
        onboard.open(&[], None);
        // 这时 versions 还在拉，busy 为真。
        onboard.handle(&json!({"kind": "onboard", "cmd": "install"}), &[]);
        assert!(onboard.busy() || onboard.queued.is_some(), "请求该被留住");

        drive(&mut onboard);
        assert!(onboard.error.is_none(), "不该失败：{:?}", onboard.error);
        assert!(paths.user_dsh().is_file(), "排队的安装最后要真的跑");
        assert!(log_text(&onboard).contains("added 42 packages"));
        assert!(onboard.env_ready(), "装完该复检成就绪");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn opening_the_wizard_creates_its_scratch_directories() {
        // 全新机器上 data/state/downloads 都不存在；curl 往不存在的目录写会以 23 退出。
        let (paths, root) = scratch("open-dirs");
        let mut onboard = Onboard::new(paths.clone(), Settings::default(), &[]);
        onboard.open(&[], None);
        assert!(paths.data.is_dir(), "数据目录要建出来");
        assert!(paths.install_downloads().is_dir(), "downloads 目录要建出来");
        assert!(paths.backups().is_dir(), "备份目录要建出来");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn install_queues_selected_plugins_after_dsh() {
        let (paths, root) = scratch("plugins-queue");
        let mut onboard = fresh(&paths);
        onboard.settings.plugins = Some(vec!["dsh-plugin-feishu".to_owned()]);
        onboard.handle(&json!({"kind": "onboard", "cmd": "install"}), &[]);

        let queue: Vec<String> = onboard.pending.iter().map(Step::label).collect();
        let dsh_at = queue
            .iter()
            .position(|label| label.starts_with("安装 dsh"))
            .expect("队列里要有 dsh");
        let plugins_at = queue
            .iter()
            .position(|label| label.contains("预装推荐插件（1 个）"))
            .expect("选中的插件要进队列");
        assert!(
            dsh_at < plugins_at,
            "插件必须排在 dsh 后面（命令是 dsh 提供的）：{queue:?}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unchecking_every_plugin_ships_nothing_extra() {
        let (paths, root) = scratch("plugins-none");
        let mut onboard = fresh(&paths);
        onboard.settings.plugins = Some(Vec::new());
        onboard.handle(&json!({"kind": "onboard", "cmd": "install"}), &[]);
        assert!(
            !onboard
                .pending
                .iter()
                .any(|step| matches!(step, Step::InstallPlugins { .. })),
            "全部取消勾选就不该预装"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn snapshot_lists_recommended_plugins_as_selected() {
        let (paths, root) = scratch("plugins-view");
        let onboard = fresh(&paths);
        let view = onboard.view();
        let recommended = view["plugins"]["recommended"]
            .as_array()
            .expect("快照要带推荐清单");
        assert_eq!(recommended.len(), dsh_setup::plugins::RECOMMENDED.len());
        assert!(
            recommended
                .iter()
                .all(|item| item["selected"].as_bool() == Some(true)),
            "默认全选"
        );
        assert_eq!(
            view["plugins"]["selected"].as_array().map(Vec::len),
            Some(recommended.len())
        );
        // 环境页之外的界面也要能读到完整选择（设置面板回显）。
        assert_eq!(
            view["settings"]["plugins"].as_array().map(Vec::len),
            Some(recommended.len())
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
