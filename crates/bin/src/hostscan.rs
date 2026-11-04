//! 扫描机器上正在运行的 `dsh` 宿主进程。
//!
//! 用来回答两个问题：**当前这个壳连的是哪个 dsh**、**有没有上次遗留的孤儿 dsh**。
//! 壳自己 fork 出来的宿主会 `setpgid` 成独立进程组，但 macOS 没有
//! `PR_SET_PDEATHSIG`，壳被 ⌘Q/强杀带走时宿主会挂到 launchd（ppid = 1）继续跑；
//! 那就是孤儿，必须能在引导页里看见并一键清掉。
//!
//! 实现上直接用 `ps`（macOS/Linux 的 `-Ao pid=,ppid=,etime=,command=` 都支持），
//! 不为这件事引入 `sysinfo` 依赖。

use std::process::Command;

/// 一个跑着的 dsh 宿主。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DshProcess {
    pub pid: u32,
    pub ppid: u32,
    /// `ps` 的 `etime`：`05:18` / `1-02:03:04`。
    pub uptime: String,
    pub command: String,
}

impl DshProcess {
    /// 孤儿：父进程已经不是壳了（`ppid == 1`，或父进程已经不存在）。
    #[must_use]
    pub fn is_orphan(&self, shell_pid: u32, host_pid: Option<u32>) -> bool {
        if Some(self.pid) == host_pid {
            return false;
        }
        self.ppid == 1 || self.ppid != shell_pid && !parent_alive(self.ppid)
    }
}

/// 命令行是不是 dsh 宿主（`node …/bin/dsh --profile …`，或者直接是 `dsh` 可执行文件）。
fn looks_like_dsh(command: &str) -> bool {
    // 排除壳自己、以及名字里只是含 `dsh` 的其它东西（例如 `dsh-desktop`）。
    if command.contains("dsh-desktop") {
        return false;
    }
    let mut parts = command.split_whitespace();
    let program = parts.next().unwrap_or_default();
    let is_dsh = |token: &str| {
        token == "dsh"
            || token.ends_with("/dsh")
            || token.ends_with("/dsh.js")
            || token.ends_with("/dsh.mjs")
    };
    if is_dsh(program) {
        return true;
    }
    // `node …/bin/dsh …`：第一个参数是可执行文件时看第二个。
    if (program.ends_with("/node") || program == "node")
        && let Some(script) = parts.next()
    {
        return is_dsh(script);
    }
    false
}

fn parent_alive(ppid: u32) -> bool {
    #[cfg(unix)]
    {
        if ppid == 0 {
            return false;
        }
        // SAFETY: `kill` 只传信号，不解引用指针。
        unsafe { libc::kill(ppid as libc::pid_t, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = ppid;
        true
    }
}

/// 扫一遍进程表。失败（`ps` 不存在 / 输出异常）就返回空表，不打扰用户。
#[must_use]
pub fn scan() -> Vec<DshProcess> {
    let output = Command::new("ps")
        .args(["-Ao", "pid=,ppid=,etime=,command="])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut found: Vec<DshProcess> = text
        .lines()
        .filter_map(|line| {
            let mut rest = line;
            let pid = take_token(&mut rest)?.parse::<u32>().ok()?;
            let ppid = take_token(&mut rest)?.parse::<u32>().ok()?;
            let uptime = take_token(&mut rest)?;
            let command = rest.trim_start().to_owned();
            looks_like_dsh(&command).then_some(DshProcess {
                pid,
                ppid,
                uptime,
                command,
            })
        })
        .collect();
    found.sort_by_key(|p| p.pid);
    found
}

/// 取下一个以空白分隔的字段，`rest` 推进到它之后。
///
/// `ps` 的输出字段用空格右对齐（`  1     0 …`），所以不能用 `split('\n')` 那一套：
/// 连续的多个空格会产生空字段。
fn take_token(rest: &mut &str) -> Option<String> {
    let trimmed = rest.trim_start();
    let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let (token, tail) = trimmed.split_at(end);
    *rest = tail;
    (!token.is_empty()).then(|| token.to_owned())
}

/// 杀掉孤儿宿主，返回清掉的数量。
///
/// 只动「确认是孤儿」的进程：`current` 那个（壳自己连着的）永远不碰。
///
/// 非 unix 平台（Windows）没有 `ps` 也没有 POSIX 信号：扫不到、也不杀，直接返回 0，
/// 由用户在任务管理器里关。
#[cfg(unix)]
pub fn reap(current: Option<u32>) -> usize {
    let shell = std::process::id();
    let mut killed = 0;
    for proc in scan() {
        if !proc.is_orphan(shell, current) {
            continue;
        }
        eprintln!(
            "dsh-desktop: 清理孤儿 dsh {}（ppid {}，已运行 {}）",
            proc.pid, proc.ppid, proc.uptime
        );
        // 宿主是独立进程组（组号 = pid），连组一起发，跟着的子进程也走。
        unsafe {
            if libc::kill(-(proc.pid as libc::pid_t), libc::SIGTERM) != 0 {
                libc::kill(proc.pid as libc::pid_t, libc::SIGTERM);
            }
        }
        killed += 1;
    }
    if killed > 0 {
        // 给它们一点时间退：界面紧接着会重扫，太快会看到「还在」。
        std::thread::sleep(std::time::Duration::from_millis(300));
        for proc in scan() {
            if proc.is_orphan(shell, current) {
                unsafe {
                    libc::kill(-(proc.pid as libc::pid_t), libc::SIGKILL);
                    libc::kill(proc.pid as libc::pid_t, libc::SIGKILL);
                }
            }
        }
    }
    killed
}

#[cfg(not(unix))]
pub fn reap(_current: Option<u32>) -> usize {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_padded_ps_rows() {
        // 真 `ps` 前面带空格，且命令行本身含空格。
        let mut rest = "   3200  1934 00:12 node /x/bin/dsh --profile web";
        assert_eq!(take_token(&mut rest).as_deref(), Some("3200"));
        assert_eq!(take_token(&mut rest).as_deref(), Some("1934"));
        assert_eq!(take_token(&mut rest).as_deref(), Some("00:12"));
        assert_eq!(rest.trim_start(), "node /x/bin/dsh --profile web");
    }

    #[test]
    fn recognises_dsh_hosts_but_not_the_shell() {
        assert!(looks_like_dsh(
            "node /Users/me/.nvm/versions/node/v24.18.0/bin/dsh --profile web --port 0"
        ));
        assert!(looks_like_dsh("/usr/local/bin/dsh --profile web"));
        assert!(!looks_like_dsh(
            "/Applications/dsh-desktop.app/Contents/MacOS/dsh-desktop"
        ));
        assert!(!looks_like_dsh("node /tmp/other.js"));
    }

    #[test]
    fn the_connected_host_is_never_an_orphan() {
        let proc = DshProcess {
            pid: 3200,
            ppid: 1,
            uptime: "00:10".to_owned(),
            command: "node /x/bin/dsh".to_owned(),
        };
        assert!(!proc.is_orphan(std::process::id(), Some(3200)));
        assert!(proc.is_orphan(std::process::id(), Some(9999)));
    }

    #[test]
    fn a_child_of_this_shell_is_not_an_orphan() {
        let proc = DshProcess {
            pid: 42,
            ppid: std::process::id(),
            uptime: "00:10".to_owned(),
            command: "node /x/bin/dsh".to_owned(),
        };
        assert!(!proc.is_orphan(std::process::id(), None));
    }
}
