//! 扫描机器上正在运行的 `dsh` 宿主进程。
//!
//! 用来回答两个问题：**当前这个壳连的是哪个 dsh**、**有没有上次遗留的孤儿 dsh**。
//! 壳自己 fork 出来的宿主会 `setpgid` 成独立进程组，但 macOS 没有
//! `PR_SET_PDEATHSIG`，壳被 ⌘Q/强杀带走时宿主会挂到 launchd（ppid = 1）继续跑；
//! 那就是孤儿，必须能在引导页里看见并一键清掉。
//!
//! 实现上不引入 `sysinfo`：macOS/Linux 用 `ps -Ao pid=,ppid=,etime=,command=`，
//! Windows 用 PowerShell 的 `Get-CimInstance Win32_Process`（只有它能拿到命令行）。

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

/// 这个 token 是不是 dsh 的入口。
///
/// 三种形态都要认：
/// - POSIX：`/x/bin/dsh`、`dsh`；
/// - POSIX 下 `node` 跑的脚本：`/x/lib/dsh.js`；
/// - Windows：`dsh.exe` / `dsh.cmd`，以及我们 0.1.20 起直接 spawn 的
///   `node.exe C:\…\node_modules\@deepseek-ai\dsh\lib\bin.js`。
fn is_dsh_entry(token: &str) -> bool {
    let token = token.trim_matches('"');
    token == "dsh"
        || token == "dsh.exe"
        || token == "dsh.cmd"
        || token.ends_with("/dsh")
        || token.ends_with("/dsh.js")
        || token.ends_with("/dsh.mjs")
        || token.ends_with("/dsh.exe")
        || token.ends_with("/dsh.cmd")
        || token.ends_with("/dsh/lib/bin.js")
}

/// 命令行是不是 dsh 宿主（`node …/bin/dsh --profile …`，或者直接是 `dsh` 可执行文件）。
fn looks_like_dsh(command: &str) -> bool {
    // Windows 的命令行是反斜杠路径，统一成 `/` 再比，省得两边各写一套。
    // 顺手把重复的分隔符折掉：JSON 里的路径是转义过的（`C:\\\\npm`），
    // 反斜杠换过来就成了 `C://npm`。
    let command = command.replace('\\', "/").replace("//", "/");
    // 排除壳自己、以及名字里只是含 `dsh` 的其它东西（例如 `dsh-desktop`）。
    if command.contains("dsh-desktop") {
        return false;
    }
    let (program, rest) = split_first_token(&command);
    if is_dsh_entry(program) {
        return true;
    }
    // `node …/bin/dsh …`：第一个参数是可执行文件时看第二个。
    let is_node = program.ends_with("/node")
        || program == "node"
        || program.ends_with("/node.exe")
        || program == "node.exe";
    if is_node {
        return is_dsh_entry(split_first_token(rest).0);
    }
    false
}

/// 切出第一个 token，尊重引号。
///
/// Windows 的程序路径常带空格且被引号包住
/// （`"C:\Program Files\nodejs\node.exe" …`），按空白硬切只会剩 `"C:/Program`。
fn split_first_token(command: &str) -> (&str, &str) {
    let trimmed = command.trim_start();
    if let Some(rest) = trimmed.strip_prefix('"')
        && let Some(end) = rest.find('"')
    {
        return (&rest[..end], &rest[end + 1..]);
    }
    match trimmed.find(char::is_whitespace) {
        Some(end) => (&trimmed[..end], &trimmed[end..]),
        None => (trimmed, ""),
    }
}

#[cfg(unix)]
fn parent_alive(ppid: u32) -> bool {
    if ppid == 0 {
        return false;
    }
    // SAFETY: `kill` 只传信号，不解引用指针。
    unsafe { libc::kill(ppid as libc::pid_t, 0) == 0 }
}

/// Windows 没有 `kill(pid, 0)` 这套探测语义，借 `tasklist` 问一句在不在。
///
/// 必须真的问：Windows 不会把孤儿进程重新挂到 pid 1，父进程死了之后
/// 子进程的 ppid 还停在那个已经消失的数字上 —— 只有「这个 ppid 还活着吗」
/// 才能判出孤儿。
#[cfg(windows)]
fn parent_alive(ppid: u32) -> bool {
    if ppid == 0 {
        return false;
    }
    let mut cmd = Command::new("tasklist");
    cmd.args(["/FI", &format!("PID eq {ppid}"), "/FO", "CSV", "/NH"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    dsh_platform::quiet(&mut cmd);
    match cmd.output() {
        Ok(out) => tasklist_lists_a_process(&String::from_utf8_lossy(&out.stdout)),
        Err(_) => true, // 问不到就别误判成孤儿（宁可不杀）
    }
}

/// `tasklist /FO CSV` 的「这个 pid 存在」形态：至少有一行带 `.exe`。
/// 查不到时输出的是本地化文案（`信息: 没有运行的任务匹配指定标准。`）。
#[cfg(any(windows, test))]
fn tasklist_lists_a_process(csv: &str) -> bool {
    csv.to_ascii_lowercase().contains(".exe")
}

/// 扫一遍进程表。失败（`ps` 不存在 / 输出异常）就返回空表，不打扰用户。
#[cfg(unix)]
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

/// Windows：`ps` 不存在，用 PowerShell 的 CIM 查询（只有 Win32_Process 带命令行）。
///
/// `@(…) | ConvertTo-Json` 保证单条结果也是数组形态；`-Depth 3` 够扁平的
/// pscustomobject 用。查询本身在 Windows 上约 300ms，点「重新扫描」才会跑。
#[cfg(windows)]
const SCAN_SCRIPT: &str = concat!(
    "@(Get-CimInstance Win32_Process -Filter \"Name='node.exe'\" | ForEach-Object { ",
    "[pscustomobject]@{ pid = $_.ProcessId; ppid = $_.ParentProcessId; ",
    "age = [int]((Get-Date) - $_.CreationDate).TotalSeconds; command = $_.CommandLine } }) ",
    "| ConvertTo-Json -Compress -Depth 3"
);

#[cfg(windows)]
#[must_use]
pub fn scan() -> Vec<DshProcess> {
    let mut cmd = Command::new("powershell");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", SCAN_SCRIPT])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    dsh_platform::quiet(&mut cmd);
    let Ok(out) = cmd.output() else {
        return Vec::new();
    };
    let mut found = parse_windows_json(&String::from_utf8_lossy(&out.stdout));
    found.sort_by_key(|p| p.pid);
    found
}

#[cfg(any(windows, test))]
fn parse_windows_json(raw: &str) -> Vec<DshProcess> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw.trim()) else {
        return Vec::new();
    };
    let items = match value {
        serde_json::Value::Array(items) => items,
        serde_json::Value::Null => Vec::new(),
        single => vec![single],
    };
    let mut found: Vec<DshProcess> = items
        .iter()
        .filter_map(|item| {
            let pid = item.get("pid")?.as_u64()? as u32;
            let ppid = item.get("ppid")?.as_u64()? as u32;
            let age = item
                .get("age")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let command = item
                .get("command")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            looks_like_dsh(&command).then(|| DshProcess {
                pid,
                ppid,
                uptime: format_uptime(age),
                command,
            })
        })
        .collect();
    found.sort_by_key(|p| p.pid);
    found
}

/// 秒数 → 和 `ps -o etime` 同形的字符串（`05:18` / `1-02:03:04`）。
#[cfg(any(windows, test))]
fn format_uptime(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;
    let secs = seconds % 60;
    if days > 0 {
        format!("{days}-{hours:02}:{minutes:02}:{secs:02}")
    } else {
        format!("{hours:02}:{minutes:02}:{secs:02}")
    }
}

/// 取下一个以空白分隔的字段，`rest` 推进到它之后。
///
/// `ps` 的输出字段用空格右对齐（`  1     0 …`），所以不能用 `split('\n')` 那一套：
/// 连续的多个空格会产生空字段。
#[cfg(any(unix, test))]
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

/// Windows 版：`taskkill /T` 收掉整棵树（node 会派生 pnpm / MCP 子进程）。
#[cfg(windows)]
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
        if dsh_supervisor::kill_tree(proc.pid) {
            killed += 1;
        }
    }
    if killed > 0 {
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    killed
}

#[cfg(not(any(unix, windows)))]
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
    fn recognises_windows_hosts_we_spawn_ourselves() {
        // 0.1.20 起 Windows 上直接 spawn node + bin.js，命令行是反斜杠 + 带引号。
        assert!(looks_like_dsh(
            r#""C:\Program Files\nodejs\node.exe" C:\Users\h\AppData\Roaming\npm\node_modules\@deepseek-ai\dsh\lib\bin.js --profile web --host 127.0.0.1"#
        ));
        assert!(looks_like_dsh(r"C:\nodejs\node.exe dsh.cmd --profile web"));
        assert!(!looks_like_dsh(
            r#""C:\Program Files\nodejs\node.exe" C:\x\some-other.js"#
        ));
        assert!(!looks_like_dsh(
            r"C:\Program Files\dsh-desktop\dsh-desktop.exe"
        ));
    }

    #[test]
    fn windows_scan_output_is_parsed_defensively() {
        // JSON 里的反斜杠必须是双写（`C:\\npm`），否则 serde 会当成非法转义直接失败。
        let array = r#"[{"pid":3200,"ppid":1,"age":3725,"command":"node C:\\npm\\node_modules\\@deepseek-ai\\dsh\\lib\\bin.js --profile web"},{"pid":42,"ppid":3200,"age":5,"command":"node C:\\x\\plugin.js"}]"#;
        let found = parse_windows_json(array);
        assert_eq!(found.len(), 1, "只认 dsh 宿主，别把别的 node 也列出来");
        assert_eq!(found[0].pid, 3200);
        assert_eq!(found[0].uptime, "01:02:05");
        // 单条结果 PowerShell 可能给对象而不是数组。
        let single = r#"{"pid":7,"ppid":1,"age":59,"command":"/x/bin/dsh --profile web"}"#;
        assert_eq!(parse_windows_json(single).len(), 1);
        // 空结果 / 报错文案都不能炸。
        assert!(parse_windows_json("").is_empty());
        assert!(parse_windows_json("Get-CimInstance : 拒绝访问").is_empty());
        assert!(parse_windows_json("null").is_empty());
    }

    #[test]
    fn tasklist_output_tells_whether_a_pid_still_exists() {
        assert!(tasklist_lists_a_process(
            "\"node.exe\",\"4242\",\"Console\",\"1\",\"91,336 K\"\r\n"
        ));
        assert!(!tasklist_lists_a_process(
            "信息: 没有运行的任务匹配指定标准。\r\n"
        ));
        assert!(!tasklist_lists_a_process(""));
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
