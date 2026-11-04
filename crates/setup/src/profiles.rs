//! dsh profile：枚举、挑「能当桌面宿主」的那个、备份 / 还原 / 导入。
//!
//! profile 是 `$DSH_HOME/profiles/<name>/` 下的一个 pnpm 项目
//! （`package.json` + `cordis.yml` + `cordis.patch.yml` + `pnpm-lock.yaml` + `node_modules`）。
//! 只有 `dsh.profile.bundles` 里含 `@deepseek-ai/dsh-web-app` 的 profile 才能起 Web UI，
//! 壳只能用它当宿主 —— 这是 `--profile` 唯一合法的取值来源。
//!
//! 归档用系统 `tar`：需要的是「一个能原样带回来的 tar.gz」，为此加两个 crate 不划算。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::install::run_blocking;
use crate::stamp::utc_stamp;

/// 能提供 Web UI 的 bundle 名。
pub const WEB_BUNDLE: &str = "@deepseek-ai/dsh-web-app";

/// `$DSH_HOME`，默认 `~/.dsh`（与上游 launcher 一致）。
#[must_use]
pub fn dsh_home() -> PathBuf {
    match std::env::var_os("DSH_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => std::env::var_os("HOME").map_or_else(
            || PathBuf::from(".dsh"),
            |home| PathBuf::from(home).join(".dsh"),
        ),
    }
}

/// `<dsh_home>/profiles`。
#[must_use]
pub fn profiles_root(dsh_home: &Path) -> PathBuf {
    dsh_home.join("profiles")
}

/// 一个 profile 的概况。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileInfo {
    pub name: String,
    pub path: PathBuf,
    pub bundles: Vec<String>,
    /// 能不能当桌面宿主。
    pub web_capable: bool,
    /// `node_modules` 在不在（还原后没依赖就起不来）。
    pub has_deps: bool,
}

/// 读 `package.json` 里的 `dsh.profile.bundles`。读不到就是空。
#[must_use]
pub fn read_bundles(dir: &Path) -> Vec<String> {
    let Ok(raw) = fs::read_to_string(dir.join("package.json")) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Vec::new();
    };
    json.pointer("/dsh/profile/bundles")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// bundle 列表里是否有 Web App。
#[must_use]
pub fn is_web_capable(bundles: &[String]) -> bool {
    bundles.iter().any(|bundle| bundle == WEB_BUNDLE)
}

/// 枚举 `<root>` 下的所有 profile（目录名排序，跳过隐藏目录）。
#[must_use]
pub fn list(root: &Path) -> Vec<ProfileInfo> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut profiles: Vec<ProfileInfo> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            // `~/.dsh/profiles` 里还有 pnpm 的 `node_modules`，它不是 profile。
            if name.starts_with('.') || name == "node_modules" {
                return None;
            }
            let path = entry.path();
            let bundles = read_bundles(&path);
            Some(ProfileInfo {
                web_capable: is_web_capable(&bundles),
                has_deps: path.join("node_modules").exists(),
                name,
                path,
                bundles,
            })
        })
        .collect();
    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    profiles
}

/// 只挑能当宿主的 profile 名。
#[must_use]
pub fn web_profiles(root: &Path) -> Vec<String> {
    list(root)
        .into_iter()
        .filter(|profile| profile.web_capable)
        .map(|profile| profile.name)
        .collect()
}

/// profile 名会被拼进路径和命令行，只放行「像目录名」的输入。
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("名称不能为空".to_owned());
    }
    if name.len() > 64 {
        return Err("名称过长（上限 64 字节）".to_owned());
    }
    let ok = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !name.starts_with('.');
    if !ok {
        return Err(format!(
            "名称只能含字母、数字、`-`、`_`、`.`，且不能以 `.` 开头：{name}"
        ));
    }
    Ok(())
}

/// 备份文件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupFile {
    pub path: PathBuf,
    pub profile: String,
    pub stamp: String,
    pub bytes: u64,
}

/// 备份一个 profile 到 `<backups>/<profile>-<stamp>.tar.gz`。
///
/// 排除 `node_modules`：里面是指向全局安装的符号链接森林，几十万行、没有备份价值，
/// 还原后用 [`repair_deps`] 重建。
pub fn backup(profile: &Path, backups: &Path, at_unix: u64) -> io::Result<PathBuf> {
    let name = profile
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "profile 路径没有名字"))?;
    crate::settings::ensure_private_dir(backups)?;
    let archive = backups.join(format!("{name}-{}.tar.gz", utc_stamp(at_unix)));
    let parent = profile
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "profile 路径没有父目录"))?;
    let result = run_blocking(
        Path::new("tar"),
        &[
            "-czf".to_owned(),
            archive.display().to_string(),
            "-C".to_owned(),
            parent.display().to_string(),
            "--exclude=node_modules".to_owned(),
            name.clone(),
        ],
        &[],
        None,
        Duration::from_secs(300),
    )?;
    if !result.success() {
        return Err(io::Error::other(result.failure_note()));
    }
    Ok(archive)
}

/// 列出备份目录里的归档。
#[must_use]
pub fn backups(dir: &Path) -> Vec<BackupFile> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<BackupFile> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let stem = file_name.strip_suffix(".tar.gz")?;
            let (profile, stamp) = stem.rsplit_once('-')?;
            let bytes = entry.metadata().ok()?.len();
            Some(BackupFile {
                path: entry.path(),
                profile: profile.to_owned(),
                stamp: stamp.to_owned(),
                bytes,
            })
        })
        .collect();
    files.sort_by(|a, b| b.stamp.cmp(&a.stamp));
    files
}

/// 还原：先把现有目录挪开（`<name>.bak-<stamp>`），解压成功才删旧目录；
/// 解压失败就把旧的挪回来 —— 还原失败不该比不还原更糟。
pub fn restore(archive: &Path, target: &Path) -> io::Result<()> {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "目标路径没有名字"))?;
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "目标路径没有父目录"))?;
    fs::create_dir_all(parent)?;

    let aside = if target.exists() {
        let aside = parent.join(format!(
            "{name}.bak-{}",
            utc_stamp(crate::stamp::now_unix())
        ));
        fs::rename(target, &aside)?;
        Some(aside)
    } else {
        None
    };

    let extracted = run_blocking(
        Path::new("tar"),
        &[
            "-xzf".to_owned(),
            archive.display().to_string(),
            "-C".to_owned(),
            parent.display().to_string(),
        ],
        &[],
        None,
        Duration::from_secs(300),
    );
    match extracted {
        Ok(result) if result.success() => {
            if let Some(aside) = aside {
                // 新旧目录同名；解压出来的就是 target。挪开的旧目录直接删掉。
                let _ = fs::remove_dir_all(aside);
            }
            Ok(())
        }
        Ok(result) => {
            if let Some(aside) = aside {
                let _ = fs::rename(aside, target);
            }
            Err(io::Error::other(result.failure_note()))
        }
        Err(err) => {
            if let Some(aside) = aside {
                let _ = fs::rename(aside, target);
            }
            Err(err)
        }
    }
}

/// 从外部归档导入成新 profile。归档顶层目录名不重要，里面的内容才是。
///
/// 先解到临时目录再原子改名，避免导入到一半留下半成品。
pub fn import(archive: &Path, root: &Path, name: &str) -> io::Result<PathBuf> {
    validate_name(name).map_err(io::Error::other)?;
    let target = root.join(name);
    if target.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("profile「{name}」已存在"),
        ));
    }
    fs::create_dir_all(root)?;

    let staging = root.join(format!(".import-{}", utc_stamp(crate::stamp::now_unix())));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging)?;

    let result = run_blocking(
        Path::new("tar"),
        &[
            "-xzf".to_owned(),
            archive.display().to_string(),
            "-C".to_owned(),
            staging.display().to_string(),
        ],
        &[],
        None,
        Duration::from_secs(300),
    )?;
    if !result.success() {
        let _ = fs::remove_dir_all(&staging);
        return Err(io::Error::other(result.failure_note()));
    }

    // 归档可能带一层顶层目录。
    let mut source = staging.clone();
    if !source.join("package.json").exists()
        && let Some(inner) = single_dir(&staging)
    {
        source = inner;
    }
    if !source.join("package.json").exists() {
        let _ = fs::remove_dir_all(&staging);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "归档里没有 package.json，不是 dsh profile",
        ));
    }

    fs::rename(&source, &target)?;
    let _ = fs::remove_dir_all(&staging);
    Ok(target)
}

fn single_dir(dir: &Path) -> Option<PathBuf> {
    let entries: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    (entries.len() == 1).then(|| entries[0].clone())
}

/// 还原后没有 `node_modules` 就需要重建依赖。
#[must_use]
pub fn needs_deps(profile: &Path) -> bool {
    !profile.join("node_modules").exists() && profile.join("package.json").exists()
}

/// 在 profile 目录里重建依赖（dsh 的 profile 本身就是 pnpm 项目）。
///
/// 真正的活交给 [`crate::pnpm::repair_deps`]：先定值 `allowBuilds` 占位符，再按
/// store 自检决定要不要整装重装 —— 直接 `pnpm install` 在 store 不一致时只会
/// `ERR_PNPM_UNEXPECTED_STORE` 退出，profile 永远修不好。
pub fn repair_deps(profile: &Path, env: &[(String, String)]) -> io::Result<crate::pnpm::Repair> {
    // Windows 上是 `pnpm.cmd`：直接 spawn `pnpm` 会得到 os error 193。
    crate::pnpm::repair_deps(profile, &crate::envcheck::resolve_program("pnpm"), env)
}

/// 目录体积（字节）。`node_modules` 跳过：它是链接森林，统计它既慢又没意义。
#[must_use]
pub fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name() != "node_modules")
        .map(|entry| {
            let path = entry.path();
            match entry.metadata() {
                Ok(meta) if meta.is_dir() => dir_size(&path),
                Ok(meta) if meta.is_file() => meta.len(),
                _ => 0,
            }
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dsh-setup-prof-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_profile(root: &Path, name: &str, bundles: &[&str]) {
        let dir = root.join(name);
        fs::create_dir_all(dir.join("node_modules")).unwrap();
        fs::write(dir.join("node_modules").join("junk"), "x").unwrap();
        fs::write(dir.join("cordis.patch.yml"), "[]\n").unwrap();
        fs::write(
            dir.join("package.json"),
            serde_json::json!({
                "name": format!("dsh-profile-{name}"),
                "dsh": { "profile": { "bundles": bundles } }
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn lists_only_web_capable_as_bootable() {
        let root = tempdir("list");
        make_profile(&root, "web", &[WEB_BUNDLE]);
        make_profile(&root, "acp", &["@deepseek-ai/dsh-acp"]);
        make_profile(&root, ".hidden", &[WEB_BUNDLE]);

        let all = list(&root);
        assert_eq!(all.len(), 2, "隐藏目录不算 profile");
        assert_eq!(all[0].name, "acp");
        assert!(!all[0].web_capable);
        assert!(all[1].web_capable);
        assert!(all[1].has_deps);
        assert_eq!(web_profiles(&root), vec!["web".to_owned()]);
    }

    #[test]
    fn missing_or_broken_package_json_yields_no_bundles() {
        let root = tempdir("broken");
        fs::create_dir_all(root.join("half")).unwrap();
        fs::write(root.join("half").join("package.json"), "{oops").unwrap();
        assert!(read_bundles(&root.join("half")).is_empty());
        assert!(list(&root).iter().all(|p| !p.web_capable));
    }

    #[test]
    fn names_are_validated() {
        assert!(validate_name("web").is_ok());
        assert!(validate_name("open-design.2").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("../etc").is_err());
        assert!(validate_name(".hidden").is_err());
        assert!(validate_name("有空格").is_err());
    }

    #[test]
    fn backup_skips_node_modules_and_restore_brings_it_back() {
        let root = tempdir("backup");
        let backups = tempdir("backups");
        make_profile(&root, "web", &[WEB_BUNDLE]);
        let profile = root.join("web");

        let archive = backup(&profile, &backups, 1_795_576_055).unwrap();
        assert_eq!(
            archive.file_name().unwrap().to_string_lossy(),
            "web-20261125T030735Z.tar.gz"
        );
        let listing = backups_of(&archive);
        assert!(listing.contains("web/cordis.patch.yml"), "清单：{listing}");
        assert!(!listing.contains("node_modules"), "node_modules 不该进归档");

        // 破坏原目录，再还原。
        fs::write(profile.join("cordis.patch.yml"), "损坏\n").unwrap();
        restore(&archive, &profile).unwrap();
        assert_eq!(
            fs::read_to_string(profile.join("cordis.patch.yml")).unwrap(),
            "[]\n"
        );
        assert!(!profile.join("node_modules").exists());
        assert!(needs_deps(&profile));
    }

    #[test]
    fn restore_failure_keeps_previous_directory() {
        let root = tempdir("restore-fail");
        make_profile(&root, "web", &[WEB_BUNDLE]);
        let profile = root.join("web");
        fs::write(profile.join("keep.txt"), "原内容").unwrap();

        let bad = root.join("bad.tar.gz");
        fs::write(&bad, "not a tarball").unwrap();
        assert!(restore(&bad, &profile).is_err());
        assert_eq!(
            fs::read_to_string(profile.join("keep.txt")).unwrap(),
            "原内容",
            "还原失败必须把旧目录挪回来"
        );
    }

    #[test]
    fn backups_are_listed_newest_first() {
        let root = tempdir("b-list");
        let backups_dir = tempdir("b-dir");
        make_profile(&root, "web", &[WEB_BUNDLE]);
        let profile = root.join("web");
        backup(&profile, &backups_dir, 1_700_000_000).unwrap();
        backup(&profile, &backups_dir, 1_795_576_055).unwrap();
        let list = backups(&backups_dir);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].stamp, "20261125T030735Z");
        assert_eq!(list[0].profile, "web");
    }

    #[test]
    fn import_accepts_wrapped_archive_and_rejects_duplicates() {
        let root = tempdir("imp-root");
        let src = tempdir("imp-src");
        let backups = tempdir("imp-b");
        make_profile(&src, "web", &[WEB_BUNDLE]);
        let archive = backup(&src.join("web"), &backups, 1_700_000_000).unwrap();

        let imported = import(&archive, &root, "web-copy").unwrap();
        assert!(imported.join("package.json").exists());
        assert_eq!(read_bundles(&imported), vec![WEB_BUNDLE.to_owned()]);
        assert!(import(&archive, &root, "web-copy").is_err(), "重名要拒绝");
        assert!(import(&archive, &root, "bad/name").is_err());
    }

    #[test]
    fn dir_size_skips_node_modules() {
        let root = tempdir("size");
        make_profile(&root, "web", &[WEB_BUNDLE]);
        fs::write(root.join("web").join("big.bin"), vec![0u8; 1024]).unwrap();
        let size = dir_size(&root.join("web"));
        assert!(size >= 1024, "实际 {size}");
        assert!(size < 10_000, "node_modules 不该被计入，实际 {size}");
    }

    fn backups_of(archive: &Path) -> String {
        let out = std::process::Command::new("tar")
            .args(["-tzf", &archive.display().to_string()])
            .output()
            .expect("tar -tzf");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}
