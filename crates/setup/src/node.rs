//! node 运行时：从 nodejs.org 下载官方 tarball、校验 SHA-256、解开到用户目录。
//!
//! 为什么装 node 而不复用系统 node：dsh 要求 node >= 24.9，
//! Ubuntu / 用户机器上的 node 常常是 18 或 20（本机 nvm 之外的 `/usr/bin/node` 就是 18）。
//! 自己装一份到 `~/.local/share/dsh-desktop-rs/node/<version>` 最省事，也不污染系统。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest as _, Sha256};

use crate::install::{CommandResult, Job, JobEvent, run_blocking};

/// 官方分发根。
pub const NODE_DIST: &str = "https://nodejs.org/dist";

/// 拿不到 LTS 清单时的兜底（同时是 `settings::FALLBACK_NODE_VERSION` 的注释来源）。
pub const FALLBACK_LTS: &str = crate::settings::FALLBACK_NODE_VERSION;

/// 这台机器的 node 发行包架构名。
#[must_use]
pub fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "arm" => "armv7l",
        "x86" => "x86",
        other => other,
    }
}

/// `node-v24.13.0-linux-x64.tar.gz`
///
/// 用 `.tar.gz` 而不是 `.tar.xz`：解压只需要 tar + gzip，不依赖 xz。
#[must_use]
pub fn asset_name(version: &str) -> String {
    format!("node-v{version}-linux-{}.tar.gz", arch())
}

/// tarball 的下载地址。
#[must_use]
pub fn tarball_url(version: &str) -> String {
    format!("{NODE_DIST}/v{version}/{}", asset_name(version))
}

/// 校验文件地址（每个版本一份 SHASUMS256.txt）。
#[must_use]
pub fn shasums_url(version: &str) -> String {
    format!("{NODE_DIST}/v{version}/SHASUMS256.txt")
}

/// 版本索引（用来找当前 LTS）。
#[must_use]
pub fn index_url() -> &'static str {
    "https://nodejs.org/dist/index.json"
}

/// 本地安装根：`<data_dir>/node`。
#[must_use]
pub fn install_root(data_dir: &Path) -> PathBuf {
    data_dir.join("node")
}

/// 某个版本的安装前缀。
#[must_use]
pub fn version_prefix(data_dir: &Path, version: &str) -> PathBuf {
    install_root(data_dir).join(format!("v{version}"))
}

/// 装好的 node 的 bin 目录（要前置进 PATH 的那个）。
#[must_use]
pub fn bin_dir(data_dir: &Path, version: &str) -> PathBuf {
    version_prefix(data_dir, version).join("bin")
}

/// 从 `index.json` 里挑最新的 LTS 版本。
///
/// index.json 是一个数组，每项形如
/// `{"version":"v24.13.0","date":"2026-01-07","lts":"Krypton","security":false,...}`；
/// 非 LTS 的项 `lts` 是 `false`。数组按版本从新到旧排。
#[must_use]
pub fn latest_lts(index_json: &str) -> Option<String> {
    let entries: serde_json::Value = serde_json::from_str(index_json).ok()?;
    let array = entries.as_array()?;
    array.iter().find_map(|entry| {
        // `lts` 为字符串才是 LTS 线。
        if !entry.get("lts")?.is_string() {
            return None;
        }
        let version = entry.get("version")?.as_str()?;
        Some(version.trim_start_matches('v').to_owned())
    })
}

/// 在 SHASUMS256 文本里找 `asset` 的摘要，与实际值比对。
///
/// 行格式：`<64 hex><两个空格><文件名>`。
pub fn verify_shasum(shasums: &str, asset: &str, actual_hex: &str) -> Result<(), String> {
    let expected = shasums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let digest = parts.next()?;
        let name = parts.next()?;
        (name == asset).then(|| digest.to_ascii_lowercase())
    });
    let Some(expected) = expected else {
        return Err(format!("SHASUMS256.txt 里没有 {asset} 的摘要"));
    };
    if expected != actual_hex.to_ascii_lowercase() {
        return Err(format!(
            "{asset} 校验失败：期望 {expected}，实际 {actual_hex}"
        ));
    }
    Ok(())
}

/// 计算文件 SHA-256（小写 hex）。
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// 用系统 `curl` 下载（和 `update.rs` 同款理由：不把 TLS 栈塞进二进制）。
///
/// 走 `Job` 而不是 `output()`，这样进度能一行行喂给界面，也能取消。
pub fn download(url: &str, dest: &Path, proxy: &[(String, String)]) -> io::Result<Job> {
    // 目标目录可能还不存在（全新机器上第一次下载时 `downloads/` 还没建），
    // 此时 curl 会以 23（Failure writing output to destination）退出。
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let args = vec![
        "-fL".to_owned(),
        // 不要进度表：引导页的日志面板会被 `% Total ... Dload Upload` 刷屏。
        // 出错时仍然由 `-S` 把原因打到 stderr。
        "--no-progress-meter".to_owned(),
        "-S".to_owned(),
        "--retry".to_owned(),
        "2".to_owned(),
        "--connect-timeout".to_owned(),
        "20".to_owned(),
        "-o".to_owned(),
        dest.display().to_string(),
        url.to_owned(),
    ];
    Job::spawn(Path::new("curl"), &args, proxy, None)
}

/// 解压 tarball 到 `prefix`，返回 `prefix/bin`。
///
/// tarball 顶层是 `node-vX-linux-arch/`，用 `--strip-components=1` 去掉。
pub fn extract(archive: &Path, prefix: &Path) -> io::Result<CommandResult> {
    fs::create_dir_all(prefix)?;
    let args = vec![
        "-xzf".to_owned(),
        archive.display().to_string(),
        "-C".to_owned(),
        prefix.display().to_string(),
        "--strip-components=1".to_owned(),
    ];
    run_blocking(Path::new("tar"), &args, &[], None, Duration::from_secs(300))
}

/// 解压后检查 `bin/node` 真的能跑，并返回它的版本。
pub fn verify_extracted(prefix: &Path) -> io::Result<String> {
    let node = prefix.join("bin").join("node");
    if !node.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("解压后找不到 {}", node.display()),
        ));
    }
    let result = run_blocking(
        &node,
        &["--version".to_owned()],
        &[],
        None,
        Duration::from_secs(30),
    )?;
    if !result.success() {
        return Err(io::Error::other(result.failure_note()));
    }
    Ok(result.stdout.trim().to_owned())
}

/// 下载 + 校验 + 解压 + 自检的完整流程，阻塞执行。
///
/// `log` 每一行调用一次，用于把进度贴到安装日志里。
pub fn install(
    data_dir: &Path,
    version: &str,
    proxy: &[(String, String)],
    mut log: impl FnMut(&str),
) -> io::Result<PathBuf> {
    let prefix = version_prefix(data_dir, version);
    if prefix.join("bin").join("node").exists() {
        log(&format!("node v{version} 已存在，跳过下载"));
        return Ok(prefix);
    }

    let downloads = data_dir.join("downloads");
    crate::settings::ensure_private_dir(&downloads)?;
    let asset = asset_name(version);
    let archive = downloads.join(&asset);

    log(&format!("下载 {asset}"));
    let mut job = download(&tarball_url(version), &archive, proxy)?;
    let outcome = job
        .wait(Duration::from_secs(1800))
        .ok_or_else(|| io::Error::other("下载超时（30 分钟）"))?;
    if !outcome.succeeded() {
        return Err(io::Error::other(format!(
            "下载失败（退出码 {:?}）：{}",
            outcome.finished.flatten(),
            tarball_url(version)
        )));
    }

    log("校验 SHA-256");
    let mut sums_job = download(
        &shasums_url(version),
        &downloads.join("SHASUMS256.txt"),
        proxy,
    )?;
    let sums = sums_job
        .wait(Duration::from_secs(120))
        .ok_or_else(|| io::Error::other("下载 SHASUMS256.txt 超时"))?;
    if !sums.succeeded() {
        return Err(io::Error::other("下载 SHASUMS256.txt 失败"));
    }
    let shasums = fs::read_to_string(downloads.join("SHASUMS256.txt"))?;
    let digest = sha256_file(&archive)?;
    verify_shasum(&shasums, &asset, &digest).map_err(io::Error::other)?;

    log(&format!("解压到 {}", prefix.display()));
    let extracted = extract(&archive, &prefix)?;
    if !extracted.success() {
        return Err(io::Error::other(extracted.failure_note()));
    }
    log("自检 node --version");
    let reported = verify_extracted(&prefix)?;
    log(&format!("node {reported} 就绪"));

    // 校验通过后删掉压缩包：几十 MB，没必要留着。
    let _ = fs::remove_file(&archive);
    Ok(prefix)
}

/// 把 `JobEvent` 翻译成日志行（给界面用）。
#[must_use]
pub fn event_line(event: &JobEvent) -> Option<String> {
    match event {
        JobEvent::Line { line, .. } => Some(line.clone()),
        JobEvent::Finished { code, cancelled } => Some(if *cancelled {
            "已取消".to_owned()
        } else {
            format!("结束（退出码 {:?}）", code)
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_names_follow_official_scheme() {
        assert_eq!(
            asset_name("24.13.0"),
            format!("node-v24.13.0-linux-{}.tar.gz", arch())
        );
        assert!(
            tarball_url("24.13.0").ends_with("/v24.13.0/node-v24.13.0-linux-x64.tar.gz")
                || tarball_url("24.13.0").contains("-linux-arm64.tar.gz")
        );
        assert!(shasums_url("24.13.0").ends_with("/v24.13.0/SHASUMS256.txt"));
    }

    const SAMPLE_INDEX: &str = r#"[
      {"version":"v25.1.0","date":"2026-05-01","files":[],"lts":false},
      {"version":"v24.13.0","date":"2026-01-07","files":[],"lts":"Krypton"},
      {"version":"v22.11.0","date":"2024-10-29","files":[],"lts":"Jod"}
    ]"#;

    #[test]
    fn picks_first_lts_entry() {
        assert_eq!(latest_lts(SAMPLE_INDEX).as_deref(), Some("24.13.0"));
        assert_eq!(latest_lts("[]"), None);
        assert_eq!(latest_lts("not json"), None);
    }

    #[test]
    fn shasum_matching_is_exact_and_case_insensitive() {
        let sums = "aaaa\nbbbb  node-v24.13.0-linux-x64.tar.gz\n";
        assert!(verify_shasum(sums, "node-v24.13.0-linux-x64.tar.gz", "BBBB").is_ok());
        let err = verify_shasum(sums, "node-v24.13.0-linux-x64.tar.gz", "aaaa").unwrap_err();
        assert!(err.contains("校验失败"));
        let err = verify_shasum(sums, "node-v22.0.0-linux-x64.tar.gz", "bbbb").unwrap_err();
        assert!(err.contains("没有"));
    }

    #[test]
    fn sha256_of_known_content() {
        let dir = std::env::temp_dir().join(format!("dsh-setup-sha-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hello.txt");
        fs::write(&file, "hello").unwrap();
        assert_eq!(
            sha256_file(&file).unwrap(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        let _ = fs::remove_file(&file);
    }

    #[test]
    fn verify_extracted_reports_missing_binary() {
        let dir = std::env::temp_dir().join(format!("dsh-setup-node-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let err = verify_extracted(&dir).unwrap_err();
        assert!(err.to_string().contains("bin/node"));
    }

    #[test]
    fn download_creates_a_missing_destination_directory() {
        // 全新机器上 `<data>/downloads` 还不存在；不先建目录 curl 会以 23 退出，
        // 用户看到的是「Failure writing output to destination」。
        let root = std::env::temp_dir().join(format!("dsh-setup-dl-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dest = root.join("downloads").join("probe.txt");
        let source = root.join("source.txt");
        fs::create_dir_all(&root).unwrap();
        fs::write(&source, "hello").unwrap();
        let url = format!("file://{}", source.display());
        let mut job = download(&url, &dest, &[]).expect("curl 该能起起来");
        let outcome = job
            .wait(std::time::Duration::from_secs(10))
            .expect("该结束");
        assert!(outcome.succeeded(), "{:?}", outcome.lines);
        assert_eq!(fs::read_to_string(&dest).unwrap(), "hello");
        let _ = fs::remove_dir_all(&root);
    }
}
