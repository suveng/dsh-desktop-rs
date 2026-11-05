//! profile 依赖的 pnpm 侧处理：store 对齐自检、`allowBuilds` 占位符定值、重建计划。
//!
//! profile 的 `node_modules` 是从某个 pnpm store **硬链**出来的，`.modules.yaml` 里记着
//! 当时的 `storeDir`。若它与当前 pnpm 的默认 store 不同，之后**任何** `pnpm install` /
//! `pnpm add` 都会直接失败：
//!
//! ```text
//! [ERR_PNPM_UNEXPECTED_STORE] Unexpected store location
//! The dependencies at ".../profiles/web/node_modules" are currently linked from the store at "…/old-store/v11".
//! pnpm now wants to use the store at "…/new-store/v11" to link dependencies.
//! ```
//!
//! 用户看到的是「插件页点更新永远报错」，而 profile 是壳创建 / 备份 / 还原的，壳就必须
//! 自己把这件事做对：先自检，再决定是「照常 install」还是「整装重装」。
//!
//! 同一份 profile 里还埋着第二颗雷：pnpm 在非交互环境下会把「要不要跑构建脚本」写成占位符
//! `protobufjs: set this to true or false`，之后**任何** install 都会 `ERR_PNPM_IGNORED_BUILDS`
//! 退出。所以重建依赖前要先把它定值。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::install::{Job, run_blocking};

/// `pnpm-workspace.yaml` 里 pnpm 留下的未决占位符（`pnpm approve-builds` 之前的形态）。
pub const PENDING_BUILD_VALUE: &str = "set this to true or false";

/// `node_modules/.modules.yaml` 里记 store 的键名。
const STORE_DIR_KEY: &str = "storeDir:";

/// `pnpm store path` 只是打印一个路径：起不来就是 pnpm 本身有问题，别等太久。
const STORE_PATH_TIMEOUT: Duration = Duration::from_secs(20);

/// 依赖自检的结论。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DepsState {
    /// 有依赖，且（能判定时）与当前 pnpm 的 store 一致。
    Ready,
    /// 没有 `node_modules`：装过、或者 profile 是刚解包出来的。
    Missing,
    /// `node_modules` 是另一套 store 链出来的：pnpm 只会报 `ERR_PNPM_UNEXPECTED_STORE`。
    StoreMismatch { recorded: PathBuf, current: PathBuf },
}

impl DepsState {
    /// 要不要整装重装（`CI=true pnpm install`）才可能自愈。
    #[must_use]
    pub fn needs_reinstall(&self) -> bool {
        matches!(self, Self::StoreMismatch { .. })
    }
}

/// 读 `node_modules/.modules.yaml` 里记的 `storeDir`（没有就是 `None`）。
#[must_use]
pub fn recorded_store(profile: &Path) -> Option<PathBuf> {
    let raw = fs::read_to_string(profile.join("node_modules").join(".modules.yaml")).ok()?;
    parse_store_dir(&raw)
}

/// 从 `.modules.yaml` 的文本里挑出 `storeDir`。格式是 pnpm 自己写的扁平 YAML：
/// `storeDir: /home/x/.pnpm-store/v11`（值可能带引号）。
#[must_use]
pub fn parse_store_dir(yaml: &str) -> Option<PathBuf> {
    yaml.lines().find_map(|line| {
        let rest = line.trim_start().strip_prefix(STORE_DIR_KEY)?;
        let value = rest.trim().trim_matches('"').trim_matches('\'');
        (!value.is_empty()).then(|| PathBuf::from(value))
    })
}

/// 当前 pnpm 在这个 profile 目录里会用的 store（`pnpm store path`）。
///
/// 必须在 profile 目录里问：pnpm 的默认 store 跟着 cwd 所在的文件系统走
/// （`/home` 与 `/tmp` 在两个分区上时给出两个不同答案）。
#[must_use]
pub fn current_store(pnpm: &Path, profile: &Path, env: &[(String, String)]) -> Option<PathBuf> {
    let args = ["store".to_owned(), "path".to_owned()];
    let result = run_blocking(pnpm, &args, env, Some(profile), STORE_PATH_TIMEOUT).ok()?;
    if !result.success() {
        return None;
    }
    result
        .stdout
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(PathBuf::from)
}

/// 依赖自检：`node_modules` 在不在、是不是当前 store 链出来的。
///
/// 判定不了的时候（缺 `.modules.yaml`、pnpm 起不来）返回 [`DepsState::Ready`]：
/// 宁可照常 `pnpm install`，也不要凭猜测把一份好依赖删了重装。
#[must_use]
pub fn inspect(profile: &Path, pnpm: &Path, env: &[(String, String)]) -> DepsState {
    if !profile.join("node_modules").exists() {
        return DepsState::Missing;
    }
    let (Some(recorded), Some(current)) =
        (recorded_store(profile), current_store(pnpm, profile, env))
    else {
        return DepsState::Ready;
    };
    if same_dir(&recorded, &current) {
        DepsState::Ready
    } else {
        DepsState::StoreMismatch { recorded, current }
    }
}

/// 两个路径是不是同一个目录（先比字面，再比 canonicalize：符号链接 / 大小写）。
fn same_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// `pnpm install` 的参数与额外环境变量。
///
/// store 自相矛盾时必须让 pnpm 真的重装：`CI=true` 是 pnpm 里「不交互、清掉现有
/// `node_modules` 重来」的开关（实测 16s / 183 包），不加它 pnpm 只会报错退出。
#[must_use]
pub fn install_plan(state: &DepsState) -> (Vec<String>, Vec<(String, String)>) {
    let args = vec!["install".to_owned(), "--prefer-offline".to_owned()];
    if state.needs_reinstall() {
        (args, vec![("CI".to_owned(), "true".to_owned())])
    } else {
        (args, Vec::new())
    }
}

/// 把 `pnpm-workspace.yaml` 里未决的 `allowBuilds` 占位符定值成 `true`。
///
/// pnpm 在没有交互终端时会把「要不要跑构建脚本」写成一句占位符，之后**任何**
/// install 都直接 `ERR_PNPM_IGNORED_BUILDS` 退出。壳重建依赖前先把它定下来，
/// 返回被定值的包名（已经是确定值时返回空）。
pub fn fix_pending_builds(profile: &Path) -> io::Result<Vec<String>> {
    let file = profile.join("pnpm-workspace.yaml");
    // 没有这个文件（老 profile）就没什么要修的。
    let Ok(raw) = fs::read_to_string(&file) else {
        return Ok(Vec::new());
    };
    let (patched, names) = patch_pending_builds(&raw);
    if names.is_empty() {
        return Ok(names);
    }
    fs::write(&file, patched)?;
    Ok(names)
}

/// [`fix_pending_builds`] 的纯文本版本：把 `key: set this to true or false` 换成
/// `key: true`，其余行原样保留（`minimumReleaseAgeExclude` 那串列表不能动）。
#[must_use]
pub fn patch_pending_builds(yaml: &str) -> (String, Vec<String>) {
    let mut names = Vec::new();
    let mut out = String::with_capacity(yaml.len());
    for line in yaml.lines() {
        let trimmed = line.trim_end();
        let patched = trimmed.find(PENDING_BUILD_VALUE).and_then(|at| {
            let head = &trimmed[..at];
            let tail = &trimmed[at + PENDING_BUILD_VALUE.len()..];
            // 值必须正好是占位符；后面还有别的内容说明不是我们认识的那一行。
            if !tail.trim().is_empty() {
                return None;
            }
            let colon = head.rfind(':')?;
            let name = head[..colon]
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_owned();
            (!name.is_empty()).then(|| (format!("{head}true"), name))
        });
        match patched {
            Some((line, name)) => {
                names.push(name);
                out.push_str(&line);
            }
            None => out.push_str(line),
        }
        out.push('\n');
    }
    (out, names)
}

/// 从 pnpm 的输出里揪出被拦下的构建脚本对应的包名。
///
/// pnpm 12 的非交互模式下不会写占位符，只在最后报一段：
/// `× installing dependencies` / `╰─▶ Ignored build scripts: protobufjs@7.6.6`
/// / `help: Run "pnpm approve-builds" ...`，退出码非 0。要自动放行就得自己解析。
#[must_use]
pub fn ignored_builds(output: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in output.lines() {
        let lower = line.to_lowercase();
        let Some(at) = lower.find("ignored build scripts:") else {
            continue;
        };
        for spec in line[at + "ignored build scripts:".len()..].split(',') {
            // 有些版本会补一句 `(postinstall)`，版本号后面也不一定干净。
            let spec = spec.split('(').next().unwrap_or_default().trim();
            if let Some(name) = package_name(spec)
                && !names.iter().any(|seen| seen == name)
            {
                names.push(name.to_owned());
            }
        }
    }
    names
}

/// `protobufjs@7.6.6` / `@scope/pkg@1.2.3` / `esbuild` → 包名；不像包名就 `None`。
fn package_name(spec: &str) -> Option<&str> {
    let spec = spec
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim_matches('`');
    if spec.is_empty() {
        return None;
    }
    // 作用域包的第一个 `@` 是前缀，真正的版本分隔符在后一个。
    let version_at = match spec.strip_prefix('@') {
        Some(rest) => rest.find('@').map(|at| at + 1),
        None => spec.find('@'),
    };
    let name = match version_at {
        Some(at) => {
            let version = &spec[at + 1..];
            // 冒号后面不是数字说明这不是版本号，整串当名字（宁可放过，不要乱写 YAML）。
            if !version.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                return None;
            }
            &spec[..at]
        }
        None => spec,
    };
    let ok = name.chars().all(|c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.' | '/' | '@')
    }) && name.chars().any(|c| c.is_ascii_alphabetic());
    ok.then_some(name)
}

/// 把包名写进 profile 的 `pnpm-workspace.yaml` 的 `allowBuilds` 白名单，返回真正新增的。
///
/// 这是 [`fix_pending_builds`] 的补充：那个只管「占位符」形态，pnpm 12 干脆不写
/// 占位符，只在报错里列出来 —— 不主动放行，插件就永远装不上（`billion-context`
/// 依赖的 `protobufjs` 需要跑 postinstall）。
pub fn allow_builds(profile: &Path, names: &[String]) -> io::Result<Vec<String>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let file = profile.join("pnpm-workspace.yaml");
    let raw = fs::read_to_string(&file).unwrap_or_default();
    let (patched, added) = merge_allow_builds(&raw, names);
    if added.is_empty() {
        return Ok(added);
    }
    fs::write(&file, patched)?;
    Ok(added)
}

/// [`allow_builds`] 的纯文本版本：合并进已有的 `allowBuilds:` 映射，没有就追加一段。
///
/// 只认顶层 `allowBuilds:`（pnpm 的写法），已有的 `true` 不重复写，显式的
/// `false` 也不覆盖 —— 那是用户自己拒绝的，我们不该替他改主意。
#[must_use]
pub fn merge_allow_builds(yaml: &str, names: &[String]) -> (String, Vec<String>) {
    // 先把占位符定值，剩下的才是要新写进白名单的。
    let (yaml, fixed) = patch_pending_builds(yaml);
    let mut lines: Vec<String> = yaml.lines().map(str::to_owned).collect();
    let mut added: Vec<String> = Vec::new();
    let header = lines
        .iter()
        .position(|line| line.trim() == "allowBuilds:" && !line.starts_with(' '));

    let mut insert_at = match header {
        Some(index) => {
            // 块结束于第一行「有内容且不缩进」的位置。
            let mut end = index + 1;
            while end < lines.len() {
                let line = &lines[end];
                if !line.trim().is_empty() && !line.starts_with(' ') {
                    break;
                }
                end += 1;
            }
            end
        }
        None => {
            if !lines.is_empty() && lines.last().is_some_and(|line| !line.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push("allowBuilds:".to_owned());
            lines.len()
        }
    };

    for name in names {
        if fixed.iter().any(|fixed| fixed == name) {
            continue;
        }
        let declared = lines.iter().any(|line| {
            line.trim_start()
                .strip_prefix(name.as_str())
                .is_some_and(|rest| rest.trim_start().starts_with(':'))
        });
        if declared {
            continue;
        }
        lines.insert(insert_at, format!("  {name}: true"));
        insert_at += 1;
        added.push(name.clone());
    }

    let mut out = lines.join("\n");
    out.push('\n');
    (out, added)
}

/// 一次依赖重建：跑着的 `pnpm install` + 这一步做的额外处理（写进引导页日志）。
pub struct Repair {
    pub job: Job,
    pub notes: Vec<String>,
}

// `Job` 里是管道和子进程，没法定；只把「这一步干了什么」打出来就够断言了。
impl std::fmt::Debug for Repair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Repair")
            .field("notes", &self.notes)
            .finish()
    }
}

/// 重建 profile 依赖：先定值 `allowBuilds` 占位符，再按 store 自检决定要不要整装。
///
/// 调用方（`crates/bin`）负责把 `notes` 写进安装日志、并在失败时把输出落盘。
pub fn repair_deps(profile: &Path, pnpm: &Path, env: &[(String, String)]) -> io::Result<Repair> {
    if !profile.join("package.json").exists() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} 里没有 package.json，不是 pnpm 项目", profile.display()),
        ));
    }
    let mut notes = Vec::new();
    match fix_pending_builds(profile) {
        Ok(names) if !names.is_empty() => {
            notes.push(format!("已把 allowBuilds 占位符定值：{}", names.join("、")));
        }
        Ok(_) => {}
        // 定值失败不致命：install 会自己再报一次 ERR_PNPM_IGNORED_BUILDS，
        // 那时日志里有原文，比这里吞掉强。
        Err(err) => notes.push(format!("修 allowBuilds 占位符失败（继续）：{err}")),
    }
    let state = inspect(profile, pnpm, env);
    if let DepsState::StoreMismatch { recorded, current } = &state {
        notes.push(format!(
            "node_modules 链接自 {}，当前 pnpm 用 {}：按整装重装",
            recorded.display(),
            current.display()
        ));
    }
    let (args, extra) = install_plan(&state);
    let mut env = env.to_vec();
    env.extend(extra);
    let job = Job::spawn(pnpm, &args, &env, Some(profile))?;
    Ok(Repair { job, notes })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dsh-setup-pnpm-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn module_yaml(store: &str) -> String {
        format!(
            "hoistedDependencies: {{}}\nincluded:\n  dependencies: true\nlayoutVersion: 5\n\
             packageManager: pnpm@11.21.0\npendingBuilds: []\nprunedAt: Wed, 04 Nov 2026 12:46:28 GMT\n\
             registries:\n  default: https://registry.npmjs.org/\nstoreDir: {store}\nvirtualStoreDir: .pnpm\n"
        )
    }

    #[test]
    fn parse_store_dir_reads_the_recorded_store() {
        let text = module_yaml("/home/x/.pnpm-store/v11");
        assert_eq!(
            parse_store_dir(&text),
            Some(PathBuf::from("/home/x/.pnpm-store/v11"))
        );
        assert_eq!(
            parse_store_dir("storeDir: \"/home/x/store/v11\"\n"),
            Some(PathBuf::from("/home/x/store/v11"))
        );
        assert_eq!(parse_store_dir("layoutVersion: 5\n"), None);
        assert_eq!(parse_store_dir("storeDir:\n"), None);
    }

    #[test]
    fn recorded_store_comes_from_modules_yaml() {
        let profile = tempdir("recorded");
        fs::create_dir_all(profile.join("node_modules")).unwrap();
        fs::write(
            profile.join("node_modules").join(".modules.yaml"),
            module_yaml("/old/store/v11"),
        )
        .unwrap();
        assert_eq!(
            recorded_store(&profile),
            Some(PathBuf::from("/old/store/v11"))
        );
        let empty = tempdir("recorded-empty");
        assert_eq!(recorded_store(&empty), None);
    }

    #[test]
    fn missing_node_modules_is_reported_as_missing() {
        let profile = tempdir("missing");
        fs::write(profile.join("package.json"), "{}").unwrap();
        // pnpm 不用真存在：`Missing` 在问 store 之前就返回了。
        let state = inspect(&profile, Path::new("/nonexistent/pnpm"), &[]);
        assert_eq!(state, DepsState::Missing);
        assert!(!state.needs_reinstall());
    }

    #[test]
    fn store_mismatch_switches_install_to_ci_reinstall() {
        let mismatch = DepsState::StoreMismatch {
            recorded: PathBuf::from("/old/store/v11"),
            current: PathBuf::from("/new/store/v11"),
        };
        let (args, env) = install_plan(&mismatch);
        assert_eq!(args, vec!["install", "--prefer-offline"]);
        assert_eq!(env, vec![("CI".to_owned(), "true".to_owned())]);
        assert!(mismatch.needs_reinstall());

        // 一致的时候不许往 env 里塞 CI：那会让正常 install 也整装重来。
        let (args, env) = install_plan(&DepsState::Ready);
        assert_eq!(args, vec!["install", "--prefer-offline"]);
        assert!(env.is_empty());
        assert!(!DepsState::Ready.needs_reinstall());
    }

    #[test]
    fn pending_build_placeholder_is_set_to_true() {
        let yaml = "\
packages:
  - .
nodeLinker: hoisted
allowBuilds:
  protobufjs: set this to true or false
  sharp: true
minimumReleaseAgeExclude:
  - dsh-plugin-feishu@0.7.1
";
        let (patched, names) = patch_pending_builds(yaml);
        assert_eq!(names, vec!["protobufjs"]);
        assert!(patched.contains("  protobufjs: true\n"), "实际：{patched}");
        // 已经定值的和别的列表都不能被改动。
        assert!(patched.contains("  sharp: true\n"));
        assert!(patched.contains("  - dsh-plugin-feishu@0.7.1\n"));
        assert!(!patched.contains(PENDING_BUILD_VALUE));
    }

    #[test]
    fn patch_is_a_noop_without_placeholders() {
        let yaml = "packages:\n  - .\nallowBuilds:\n  protobufjs: true\n";
        let (patched, names) = patch_pending_builds(yaml);
        assert!(names.is_empty());
        assert_eq!(patched, yaml);
    }

    #[test]
    fn fix_pending_builds_rewrites_the_workspace_file() {
        let profile = tempdir("builds");
        let file = profile.join("pnpm-workspace.yaml");
        fs::write(
            &file,
            "packages:\n  - .\nallowBuilds:\n  protobufjs: set this to true or false\n",
        )
        .unwrap();
        assert_eq!(fix_pending_builds(&profile).unwrap(), vec!["protobufjs"]);
        let text = fs::read_to_string(&file).unwrap();
        assert!(text.contains("protobufjs: true"));
        // 第二次就该是空操作（不能反复写文件）。
        assert!(fix_pending_builds(&profile).unwrap().is_empty());
        // 没有 pnpm-workspace.yaml 的 profile 不报错。
        let bare = tempdir("builds-bare");
        assert!(fix_pending_builds(&bare).unwrap().is_empty());
    }

    #[test]
    fn ignored_builds_reads_the_pnpm_12_error() {
        let output = "Progress: resolved 13, reused 11, downloaded 2, added 12\n\
\nError: ERR_PNPM_IGNORED_BUILDS\n\n  × installing dependencies\n\
  ╰─▶ Ignored build scripts: protobufjs@7.6.6\n\
  help: Run \"pnpm approve-builds\" to pick which dependencies should be allowed\n";
        assert_eq!(ignored_builds(output), vec!["protobufjs"]);
        // 多个 + 作用域包 + 括号后缀。
        assert_eq!(
            ignored_builds(
                "Ignored build scripts: esbuild@0.21.5, @swc/core@1.7.0, sharp@0.33.4 (postinstall)"
            ),
            vec!["esbuild", "@swc/core", "sharp"]
        );
        // 没有版本号也认；不是包名的整串丢掉。
        assert_eq!(
            ignored_builds("Ignored build scripts: esbuild"),
            vec!["esbuild"]
        );
        assert!(ignored_builds("Ignored build scripts: ../ evil").is_empty());
        assert!(ignored_builds("nothing to see here").is_empty());
    }

    #[test]
    fn allow_builds_appends_a_block_to_a_file_without_one() {
        let profile = tempdir("allow-new");
        assert!(allow_builds(&profile, &["protobufjs".to_owned()]).unwrap() == vec!["protobufjs"]);
        let text = fs::read_to_string(profile.join("pnpm-workspace.yaml")).unwrap();
        assert_eq!(text, "allowBuilds:\n  protobufjs: true\n");
        // 第二次是空操作。
        assert!(
            allow_builds(&profile, &["protobufjs".to_owned()])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn allow_builds_merges_into_the_existing_block() {
        let yaml = "packages:\n  - .\nallowBuilds:\n  esbuild: true\n  sharp: false\nminimumReleaseAgeExclude:\n  - '@deepseek-ai/dsh'\n";
        let (merged, added) = merge_allow_builds(
            yaml,
            &[
                "protobufjs".to_owned(),
                "esbuild".to_owned(),
                "sharp".to_owned(),
            ],
        );
        assert_eq!(added, vec!["protobufjs"]);
        assert!(merged.contains("allowBuilds:\n  esbuild: true\n  sharp: false\n  protobufjs: true\nminimumReleaseAgeExclude:"));
        // 用户的显式 false 不被改动。
        assert!(merged.contains("sharp: false"));
        // 占位符形态也一并定值。
        let placeholder = "allowBuilds:\n  protobufjs: set this to true or false\n";
        let (merged, added) = merge_allow_builds(placeholder, &["protobufjs".to_owned()]);
        assert!(added.is_empty(), "占位符已经在 fix_pending_builds 里定值了");
        assert!(merged.contains("protobufjs: true"));
        assert!(!merged.contains("set this to true or false"));
    }

    #[test]
    fn repair_refuses_a_directory_without_package_json() {
        let profile = tempdir("nopkg");
        let err = repair_deps(&profile, Path::new("/nonexistent/pnpm"), &[]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    /// 真机自检（默认跳过）：用本机 pnpm 验证 `inspect` 真能看出 store 不一致。
    ///
    /// CI 上没有 pnpm，所以标 `#[ignore]`；本地排查时这样跑：
    /// `cargo test -p dsh-setup --lib pnpm::tests::real_pnpm -- --ignored`
    #[test]
    #[ignore = "需要本机 pnpm"]
    fn real_pnpm_detects_store_mismatch() {
        let profile = tempdir("real");
        fs::create_dir_all(profile.join("node_modules")).unwrap();
        fs::write(profile.join("package.json"), "{\"name\":\"probe\"}").unwrap();
        fs::write(profile.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").unwrap();
        let pnpm = crate::envcheck::resolve_program("pnpm");
        let current = current_store(&pnpm, &profile, &[]).expect("pnpm store path");

        let modules = profile.join("node_modules").join(".modules.yaml");
        fs::write(&modules, module_yaml("/nonexistent/old-store/v11")).unwrap();
        match inspect(&profile, &pnpm, &[]) {
            DepsState::StoreMismatch {
                recorded,
                current: got,
            } => {
                assert_eq!(recorded, PathBuf::from("/nonexistent/old-store/v11"));
                assert_eq!(got, current);
            }
            other => panic!("应该判成 store 不一致，实际 {other:?}"),
        }

        fs::write(&modules, module_yaml(&current.display().to_string())).unwrap();
        assert_eq!(inspect(&profile, &pnpm, &[]), DepsState::Ready);
    }
}
