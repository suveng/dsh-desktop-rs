//! 环境检测：node / dsh 在不在、什么版本、从哪来。
//!
//! 检测只依赖两件事：文件是否可执行，以及 `<program> --version` 的输出。
//! 所有候选路径都由调用方给全（`extra`），这样测试可以塞假的可执行文件进去。

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::settings::{MAX_HOST_VERSION, MIN_HOST_VERSION, MIN_NODE_VERSION};
use crate::version::Version;

/// dsh 是从哪个位置找到的。决定「装完之后谁会赢」。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// 开发树里的 `apps/cli/lib/bin.js`（`--host-cwd` 指向 dsh 仓库时）。
    Workspace,
    /// 引导装出来的用户前缀 `~/.local/share/dsh-desktop-rs/host/bin/dsh`。
    UserPrefix,
    /// `npm prefix -g` 下的 `dsh`：全新机器上「优先全局安装」装完就落在这里。
    NpmGlobal,
    /// 向导自己装的那份 node 的全局前缀（`<data>/node/v*/bin/dsh`）。
    ///
    /// 全新机器上「优先全局安装」会落到这里：app 装的 node 一定可写，不会触发用户前缀兜底，
    /// 而这条路径又不在 PATH 上，只能按目录扫出来。
    NodePrefix,
    /// PATH 上的 `dsh`。
    Path,
    /// 兜底的 `pnpm dsh`（需要在 dsh 源码树里）。
    Pnpm,
}

impl Source {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Workspace => "开发树",
            Self::UserPrefix => "用户目录安装",
            Self::NpmGlobal => "全局安装（npm -g）",
            Self::NodePrefix => "随壳安装的 node",
            Self::Path => "PATH",
            Self::Pnpm => "pnpm",
        }
    }
}

/// 找到的 dsh。
#[derive(Clone, Debug)]
pub struct DshRuntime {
    pub program: PathBuf,
    /// 程序名前面的参数：`pnpm dsh` 时是 `["dsh"]`，直接跑可执行文件时为空。
    pub leading_args: Vec<String>,
    /// `--version` 解析出来的版本；跑了但解析不出（例如老版本不打印）时为 `None`。
    pub version: Option<Version>,
    pub source: Source,
}

impl DshRuntime {
    /// 版本落在支持区间内。
    #[must_use]
    pub fn supported(&self) -> bool {
        self.version
            .as_ref()
            .is_some_and(|version| version.in_range(&min_host(), &max_host()))
    }

    /// 版本已知但不在支持区间。
    #[must_use]
    pub fn out_of_range(&self) -> bool {
        self.version.is_some() && !self.supported()
    }

    /// 命令行（脱敏前），日志与诊断用。
    #[must_use]
    pub fn command_line(&self) -> String {
        let mut text = self.program.display().to_string();
        for arg in &self.leading_args {
            text.push(' ');
            text.push_str(arg);
        }
        text
    }
}

/// node 的情况。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeRuntime {
    /// PATH（含 extra）上找不到 node。
    Missing,
    /// 找到了但版本低于 dsh 的要求。
    TooOld { program: PathBuf, version: Version },
    /// 可用。
    Ready { program: PathBuf, version: Version },
}

impl NodeRuntime {
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }
}

#[must_use]
pub fn min_host() -> Version {
    Version::parse(MIN_HOST_VERSION).expect("MIN_HOST_VERSION 必须是合法版本号")
}

#[must_use]
pub fn max_host() -> Version {
    Version::parse(MAX_HOST_VERSION).expect("MAX_HOST_VERSION 必须是合法版本号")
}

#[must_use]
pub fn min_node() -> Version {
    Version::parse(MIN_NODE_VERSION).expect("MIN_NODE_VERSION 必须是合法版本号")
}

/// 文件存在且可执行（Unix 上看 `x` 位；其它平台只检查存在）。
#[must_use]
pub fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
    }
}

/// 在单个目录里找程序，按平台补齐扩展名。
///
/// Windows 上 `npm` / `pnpm` / `dsh` 这几个都是 **批处理包装脚本**（`npm.cmd`），
/// 同位还存在一个无扩展名的 `npm`（给 sh 用的）。直接 spawn 那个无扩展名的文件
/// 只会在 `Command::spawn` 时报 `os error 193（不是有效的 Win32 应用程序）` ——
/// 引导页「拉取版本列表失败」就是这么来的（0.1.7 的 Windows 报障）。
#[must_use]
pub fn program_in(dir: &Path, name: &str) -> Option<PathBuf> {
    program_in_impl(dir, name)
}

#[cfg(windows)]
fn program_in_impl(dir: &Path, name: &str) -> Option<PathBuf> {
    // .exe 优先（原生程序），其次 .cmd / .bat（npm / pnpm / dsh 都是这种）。
    // 无扩展名的 `npm` 是给 sh 用的文本脚本，凑上去只会得到 error 193。
    ["exe", "cmd", "bat"]
        .iter()
        .map(|ext| dir.join(format!("{name}.{ext}")))
        .find(|candidate| is_executable(candidate))
}

#[cfg(not(windows))]
fn program_in_impl(dir: &Path, name: &str) -> Option<PathBuf> {
    let candidate = dir.join(name);
    is_executable(&candidate).then_some(candidate)
}

/// 只在给定目录里找。
#[must_use]
pub fn find_in_dirs(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter().find_map(|dir| program_in(dir, name))
}

/// 只在 `PATH` 里找。
#[must_use]
pub fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| program_in(&dir, name))
}

/// 把程序名解析成能直接 spawn 的路径（Windows 上会补出 `.cmd`）。
/// 找不到就原样返回名字，让调用方的 `spawn` 报出它自己的错误。
#[must_use]
pub fn resolve_program(name: &str) -> PathBuf {
    find_in_path(name).unwrap_or_else(|| PathBuf::from(name))
}

/// 在 `extra` 目录（优先）和 `PATH` 里找可执行文件。
#[must_use]
pub fn find_executable(name: &str, extra: &[PathBuf]) -> Option<PathBuf> {
    find_in_dirs(name, extra).or_else(|| find_in_path(name))
}

/// 把 `extra` 前置到 PATH 前面。子进程（尤其是 `#!/usr/bin/env node` 的脚本）要靠它找 node。
#[must_use]
pub fn path_with(extra: &[PathBuf]) -> OsString {
    let mut dirs: Vec<PathBuf> = extra.to_vec();
    if let Some(existing) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(dirs).unwrap_or_else(|_| OsString::new())
}

/// 跑 `<program> --version`，返回解析出的版本与原始输出。
///
/// 返回 `None` 表示**跑不起来**（不是没权限、就是缺解释器）；
/// 跑起来了但输出不像版本号时返回 `(None, 输出)`。
#[must_use]
pub fn run_version(
    program: &Path,
    leading: &[String],
    extra: &[PathBuf],
) -> Option<(Option<Version>, String)> {
    let output = match exec_retrying_busy(|| {
        let mut cmd = Command::new(program);
        // Windows 上别为每次 `--version` 探测闪一个控制台窗口。
        dsh_platform::quiet(&mut cmd);
        cmd.args(leading)
            .arg("--version")
            .env("PATH", path_with(extra))
            .output()
    }) {
        Ok(output) => output,
        Err(err) => {
            // `DSH_TRACE_VER=1` 时把探测失败的原因打出来，排查「明明装了却报未安装」。
            if std::env::var_os("DSH_TRACE_VER").is_some() {
                eprintln!("run_version {:?} 失败：{err:?}", program);
            }
            return None;
        }
    };
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    // 有些工具把版本打到 stderr，都看一下。
    let raw = if stdout.is_empty() { stderr } else { stdout };
    Some((Version::parse_from_output(&raw), raw))
}

/// 撞上 `ETXTBSY` 就重试几次。
///
/// `ExecutableFileBusy` 是**瞬时**错误：多线程进程里只要有别的线程 fork，
/// 我们刚写完（或 npm 刚替换完）的可执行文件在那一小段时间里就带着写 fd 被继承了，
/// exec 会被内核拒绝。npm 自己就是先写临时文件再 rename，替换 `node` / `dsh` 的瞬间撞上很正常。
pub fn exec_retrying_busy<T>(
    mut attempt: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let mut last = None;
    for wait in [0u64, 20, 60, 150, 300] {
        if wait > 0 {
            std::thread::sleep(std::time::Duration::from_millis(wait));
        }
        match attempt() {
            Ok(value) => return Ok(value),
            Err(err) if err.kind() == std::io::ErrorKind::ExecutableFileBusy => last = Some(err),
            Err(err) => return Err(err),
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("exec 失败但没拿到错误")))
}

/// 判定 node 可用性。`extra` 一般是已装好的 node 的 bin 目录。
#[must_use]
pub fn detect_node(extra: &[PathBuf]) -> NodeRuntime {
    detect_node_with(extra, true)
}

/// 同 [`detect_node`]，但可以关掉 `PATH` 兜底 —— 测试用，避免真机上的 node 混进来。
#[must_use]
pub fn detect_node_with(extra: &[PathBuf], include_path: bool) -> NodeRuntime {
    let found = if include_path {
        find_executable("node", extra)
    } else {
        find_in_dirs("node", extra)
    };
    let Some(program) = found else {
        return NodeRuntime::Missing;
    };
    let Some((Some(version), _)) = run_version(&program, &[], extra) else {
        // 能跑起来但报不出可解析的版本：当作太旧，让用户重装比冒险启动便宜。
        return NodeRuntime::TooOld {
            program,
            version: Version::parse("0.0.0").expect("0.0.0 合法"),
        };
    };
    if version < min_node() {
        return NodeRuntime::TooOld { program, version };
    }
    NodeRuntime::Ready { program, version }
}

/// 一个候选宿主：程序、前置参数、来源。
///
/// 前置参数是必要的：开发树里的宿主是 `node apps/cli/lib/bin.js`，
/// 光拿 `node` 去问 `--version` 只会拿到 node 自己的版本号。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub program: PathBuf,
    pub leading_args: Vec<String>,
    pub source: Source,
}

impl Candidate {
    #[must_use]
    pub fn new(program: impl Into<PathBuf>, source: Source) -> Self {
        // `pnpm dsh`：`dsh` 是参数，不是程序名。
        let leading_args = if source == Source::Pnpm {
            vec!["dsh".to_owned()]
        } else {
            Vec::new()
        };
        Self {
            program: program.into(),
            leading_args,
            source,
        }
    }

    /// `node <script>` 这类候选。
    #[must_use]
    pub fn with_args(mut self, args: impl IntoIterator<Item = String>) -> Self {
        self.leading_args = args.into_iter().collect();
        self
    }

    /// 解析成能直接 exec 的路径；不存在就 `None`。
    fn resolve(&self, extra: &[PathBuf]) -> Option<PathBuf> {
        if is_executable(&self.program) {
            return Some(self.program.clone());
        }
        // 只给了名字（`node` / `pnpm` / `dsh`）的，先看装好的 node 目录，再看 PATH。
        find_executable(&self.program.to_string_lossy(), extra)
    }
}

/// 按优先级逐个试候选，返回第一个存在的。
#[must_use]
pub fn detect_dsh(candidates: &[Candidate], extra: &[PathBuf]) -> Option<DshRuntime> {
    for candidate in candidates {
        let Some(program) = candidate.resolve(extra) else {
            continue;
        };
        // 存在但跑不起来（比如 node 缺失）—— 仍然报告它，让向导去装 node。
        let version =
            run_version(&program, &candidate.leading_args, extra).and_then(|(version, _)| version);
        return Some(DshRuntime {
            program,
            leading_args: candidate.leading_args.clone(),
            version,
            source: candidate.source,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dsh-setup-env-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_tool(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        // 先写临时文件再改名：并行测试里别的线程一 fork，我们这边的写 fd 就会被继承，
        // 直接 exec 同名文件会撞上 ETXTBSY（ExecutableFileBusy）。改名后写的还是旧 inode。
        let staging = dir.join(format!(".{name}.staging"));
        fs::write(&staging, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&staging, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::rename(&staging, &path).unwrap();
        path
    }

    #[test]
    fn find_executable_prefers_extra_dirs() {
        let dir = tempdir("find");
        let tool = fake_tool(&dir, "node", "echo v24.13.0");
        assert_eq!(find_in_dirs("node", std::slice::from_ref(&dir)), Some(tool));
        assert_eq!(find_in_dirs("dsh-in-no-such-dir", &[dir]), None);
    }

    #[test]
    fn old_node_is_flagged() {
        let dir = tempdir("old-node");
        fake_tool(&dir, "node", "echo v20.11.1");
        match detect_node_with(&[dir], false) {
            NodeRuntime::TooOld { version, .. } => {
                assert_eq!(version.to_string(), "20.11.1");
            }
            other => panic!("期望 TooOld，实际 {other:?}"),
        }
    }

    #[test]
    fn modern_node_is_ready() {
        let dir = tempdir("new-node");
        let tool = fake_tool(&dir, "node", "echo v24.13.0");
        assert_eq!(
            detect_node_with(&[dir], false),
            NodeRuntime::Ready {
                program: tool,
                version: Version::parse("24.13.0").unwrap()
            }
        );
    }

    #[test]
    fn missing_node_is_missing() {
        let dir = tempdir("no-node");
        assert_eq!(detect_node_with(&[dir], false), NodeRuntime::Missing);
    }

    #[test]
    fn dsh_version_range_decides_support() {
        let dir = tempdir("dsh-range");
        let tool = fake_tool(&dir, "dsh", "echo 0.2.2-alpha.3");
        let runtime = detect_dsh(&[Candidate::new(tool.clone(), Source::UserPrefix)], &[]).unwrap();
        assert!(runtime.supported());
        assert!(!runtime.out_of_range());
        assert_eq!(runtime.source.label(), "用户目录安装");

        let old = fake_tool(&dir, "dsh-old", "echo 0.1.1-rc.2");
        let runtime = detect_dsh(&[Candidate::new(old, Source::Path)], &[]).unwrap();
        assert!(!runtime.supported());
        assert!(runtime.out_of_range());
    }

    #[test]
    fn dsh_first_candidate_wins() {
        let dir = tempdir("dsh-order");
        let first = fake_tool(&dir, "dsh-a", "echo 0.2.2-alpha.2");
        let second = fake_tool(&dir, "dsh-b", "echo 0.2.2-alpha.3");
        let runtime = detect_dsh(
            &[
                Candidate::new(first.clone(), Source::Workspace),
                Candidate::new(second, Source::Path),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(runtime.program, first);
        assert_eq!(runtime.source, Source::Workspace);
    }

    #[test]
    fn candidate_leading_args_are_probed() {
        // 开发树：宿主是 `node <script>`，版本号得从脚本拿到，而不是从 node 拿到。
        let dir = tempdir("leading");
        let runner = fake_tool(&dir, "runner", "echo \"v$1\"");
        let runtime = detect_dsh(
            &[Candidate::new(runner.clone(), Source::Workspace)
                .with_args(["0.2.2-alpha.3".to_owned()])],
            &[],
        )
        .unwrap();
        assert_eq!(runtime.program, runner);
        assert_eq!(
            runtime.version.map(|v| v.to_string()),
            Some("0.2.2-alpha.3".to_owned())
        );
        assert!(runtime.leading_args.contains(&"0.2.2-alpha.3".to_owned()));
    }

    #[test]
    fn pnpm_candidate_keeps_leading_arg() {
        let dir = tempdir("pnpm");
        let tool = fake_tool(&dir, "pnpm", "echo 0.2.2-alpha.3");
        let runtime = detect_dsh(&[Candidate::new(tool, Source::Pnpm)], &[]).unwrap();
        assert_eq!(runtime.leading_args, vec!["dsh".to_owned()]);
        assert!(runtime.command_line().ends_with("pnpm dsh"));
    }
}
