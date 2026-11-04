//! 安装任务：跑一个子进程，把它的 stdout/stderr 拆成行事件，支持取消。
//!
//! 为什么不用 `Command::output()`：npm 装一个 dsh 可能跑几分钟，
//! 期间必须能把日志实时喂给「可收起的安装日志」面板，也要能按取消键把它停掉。
//!
//! 上层（`crates/bin`）每帧调一次 [`Job::poll`]，不阻塞 UI 线程。

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::thread;
use std::time::{Duration, Instant};

/// 日志来自哪个流。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// 任务事件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobEvent {
    Line {
        stream: Stream,
        line: String,
    },
    /// 子进程结束。`code` 为 `None` 表示被信号杀掉。
    Finished {
        code: Option<i32>,
        cancelled: bool,
    },
}

/// 一次 `poll` 之后的状态汇总，方便界面判断该画什么。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JobOutcome {
    pub lines: Vec<(Stream, String)>,
    /// 已结束时的退出码（`Some(None)` 表示被信号杀掉）。
    pub finished: Option<Option<i32>>,
    pub cancelled: bool,
}

impl JobOutcome {
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.finished.is_some()
    }

    #[must_use]
    pub fn succeeded(&self) -> bool {
        matches!(self.finished, Some(Some(0)))
    }
}

enum Message {
    Line(Stream, String),
    /// 读取线程收尾（子进程关闭了管道）。带上是哪个流，方便调试时确认两条都收干净。
    Eof(Stream),
}

/// 一个正在运行的安装子进程。
pub struct Job {
    child: Child,
    rx: Receiver<Message>,
    cancel: Arc<AtomicBool>,
    /// 还在往通道里写字的读取线程数。
    readers: Arc<AtomicUsize>,
    cancelled: bool,
    /// 已经对上层「报过」结束（见 [`Job::drained`]）。
    finished: Option<Option<i32>>,
    /// 子进程自己的退出码，可能比读取线程先到。
    exit: Option<Option<i32>>,
    exited_at: Option<Instant>,
    stderr_tail: String,
    /// 收到过几条 EOF（stdout / stderr 各一条）。
    eofs: usize,
}

impl Job {
    /// 启动。`env` 会整体注入（代理、registry、PATH 都走这里）。
    pub fn spawn(
        program: &Path,
        args: &[String],
        env: &[(String, String)],
        cwd: Option<&Path>,
    ) -> std::io::Result<Self> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        for (key, value) in env {
            cmd.env(key, value);
        }
        // 独立进程组：取消时能连 npm 拉起的子进程一起收掉。
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
        }

        // `ETXTBSY` 是瞬时的（npm 替换可执行文件的瞬间会撞上），重试几次即可。
        let mut child = crate::envcheck::exec_retrying_busy(|| cmd.spawn())?;
        let (tx, rx) = channel();
        let readers = Arc::new(AtomicUsize::new(0));
        if let Some(stdout) = child.stdout.take() {
            spawn_reader(stdout, Stream::Stdout, tx.clone(), Arc::clone(&readers));
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_reader(stderr, Stream::Stderr, tx, Arc::clone(&readers));
        }
        Ok(Self {
            child,
            rx,
            cancel: Arc::new(AtomicBool::new(false)),
            readers,
            cancelled: false,
            finished: None,
            exit: None,
            exited_at: None,
            stderr_tail: String::new(),
            eofs: 0,
        })
    }

    /// 把一个事件记进 outcome / stderr 尾部。
    fn absorb(&mut self, outcome: &mut JobOutcome, message: Message) {
        match message {
            Message::Line(stream, line) => {
                if stream == Stream::Stderr && self.stderr_tail.len() < 4000 {
                    self.stderr_tail.push_str(&line);
                    self.stderr_tail.push('\n');
                }
                outcome.lines.push((stream, line));
            }
            // EOF 只记账：以前这里 `break`，两条流谁先 EOF 就把另一条已经排队的行丢了。
            Message::Eof(stream) => {
                debug_assert!(matches!(stream, Stream::Stdout | Stream::Stderr));
                self.eofs += 1;
            }
        }
    }

    /// 取走目前到齐的所有事件。不阻塞。
    pub fn poll(&mut self) -> JobOutcome {
        let mut outcome = JobOutcome::default();
        while let Ok(message) = self.rx.try_recv() {
            self.absorb(&mut outcome, message);
        }

        if self.exit.is_none() {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.exit = Some(status.code());
                    self.exited_at = Some(Instant::now());
                }
                Ok(None) => {}
                Err(_) => {
                    self.exit = Some(None);
                    self.exited_at = Some(Instant::now());
                }
            }
            if self.exit.is_some() {
                // 子进程刚退出，读取线程可能还差最后几行：给一小段时间抽干。
                let deadline = Instant::now() + Duration::from_millis(300);
                while Instant::now() < deadline {
                    while let Ok(message) = self.rx.try_recv() {
                        self.absorb(&mut outcome, message);
                    }
                    // 两个读取线程都收工了才算抽干净。
                    if self.drained() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }

        // 只有「输出已经抽干净」才向上层报结束：并行跑测试、机器很卡的时候，
        // 读取线程可能比子进程晚几十毫秒，早报结束会把最后几行（包括 `node --version`
        // 的输出）丢掉。收尾线程卡死（子进程留了孙进程占着管道）时给 5 秒兜底。
        let give_up = self
            .exited_at
            .is_some_and(|at| at.elapsed() > Duration::from_secs(5));
        if self.finished.is_none()
            && let Some(code) = self.exit
            && (self.drained() || give_up)
        {
            self.finished = Some(code);
        }

        outcome.finished = self.finished;
        outcome.cancelled = self.cancelled;
        outcome
    }

    /// 两条输出流都读到 EOF、读取线程也退了。
    fn drained(&self) -> bool {
        self.readers.load(Ordering::Relaxed) == 0 && self.eofs >= 2
    }

    /// 阻塞等到结束，最多等 `limit`。超时返回 `None`（任务继续跑）。
    pub fn wait(&mut self, limit: Duration) -> Option<JobOutcome> {
        let deadline = Instant::now() + limit;
        loop {
            let outcome = self.poll();
            if outcome.is_done() {
                return Some(outcome);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// 取消：先 SIGTERM 整个进程组，1 秒后还在就 SIGKILL。
    pub fn cancel(&mut self) {
        if self.finished.is_some() || self.cancelled {
            return;
        }
        self.cancelled = true;
        self.cancel.store(true, Ordering::Relaxed);
        #[cfg(unix)]
        {
            let pid = self.child.id();
            // 负 pid = 整个进程组。
            // SAFETY: kill 只读两个整数。
            unsafe {
                libc::kill(-(pid as i32), libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_secs(1);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = self.child.try_wait() {
                    return;
                }
                thread::sleep(Duration::from_millis(20));
            }
            // SAFETY: 同上。
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        #[cfg(not(unix))]
        {
            let _ = self.child.kill();
        }
    }

    /// stderr 尾部，出错时贴进诊断信息。
    #[must_use]
    pub fn stderr_tail(&self) -> &str {
        &self.stderr_tail
    }

    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

fn spawn_reader<R: Read + Send + 'static>(
    source: R,
    stream: Stream,
    tx: std::sync::mpsc::Sender<Message>,
    readers: Arc<AtomicUsize>,
) {
    readers.fetch_add(1, Ordering::Relaxed);
    thread::spawn(move || {
        for line in BufReader::new(source).lines() {
            let Ok(line) = line else { break };
            if tx.send(Message::Line(stream, line)).is_err() {
                break;
            }
        }
        let _ = tx.send(Message::Eof(stream));
        readers.fetch_sub(1, Ordering::Relaxed);
    });
}

/// 一次性的命令行（例如 `npm --version`、`tar -xzf`）：跑完拿结果，没有实时日志需求。
#[derive(Clone, Debug)]
pub struct CommandResult {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CommandResult {
    #[must_use]
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// 失败时给用户看的说明：优先 stderr 最后一行。
    #[must_use]
    pub fn failure_note(&self) -> String {
        let detail = self
            .stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .map(|line| line.trim().to_owned())
            .filter(|line| !line.is_empty())
            .unwrap_or_else(|| format!("退出码 {:?}", self.code));
        format!(
            "执行 `{}` 失败：{detail}",
            self.program.file_name().map_or_else(
                || self.program.display().to_string(),
                |n| n.to_string_lossy().into_owned()
            )
        )
    }
}

/// 跑一条命令并等它结束（带超时）。超时就杀掉并返回 `TimedOut`。
pub fn run_blocking(
    program: &Path,
    args: &[String],
    env: &[(String, String)],
    cwd: Option<&Path>,
    timeout: Duration,
) -> std::io::Result<CommandResult> {
    let mut job = Job::spawn(program, args, env, cwd)?;
    let mut stdout = String::new();
    let mut stderr = String::new();
    let code;
    let deadline = Instant::now() + timeout;
    loop {
        let outcome = job.poll();
        for (stream, line) in outcome.lines {
            let sink = if stream == Stream::Stdout {
                &mut stdout
            } else {
                &mut stderr
            };
            sink.push_str(&line);
            sink.push('\n');
        }
        if let Some(finished) = outcome.finished {
            code = finished;
            break;
        }
        if Instant::now() >= deadline {
            job.cancel();
            let _ = job.wait(Duration::from_secs(2));
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("命令超时（{}s）：{}", timeout.as_secs(), program.display()),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(CommandResult {
        program: program.to_path_buf(),
        args: args.to_vec(),
        code,
        stdout,
        stderr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_lines_and_exit_code() {
        let mut job = Job::spawn(
            Path::new("/bin/sh"),
            &["-c".to_owned(), "echo 一; echo 二 >&2; exit 3".to_owned()],
            &[],
            None,
        )
        .unwrap();
        let outcome = job.wait(Duration::from_secs(5)).expect("应当很快结束");
        let stdout: Vec<&str> = outcome
            .lines
            .iter()
            .filter(|(s, _)| *s == Stream::Stdout)
            .map(|(_, l)| l.as_str())
            .collect();
        let stderr: Vec<&str> = outcome
            .lines
            .iter()
            .filter(|(s, _)| *s == Stream::Stderr)
            .map(|(_, l)| l.as_str())
            .collect();
        assert_eq!(stdout, vec!["一"]);
        assert_eq!(stderr, vec!["二"]);
        assert_eq!(outcome.finished, Some(Some(3)));
        assert!(!outcome.succeeded());
        assert!(job.stderr_tail().contains('二'));
    }

    #[test]
    fn env_is_injected() {
        let mut job = Job::spawn(
            Path::new("/bin/sh"),
            &[
                "-c".to_owned(),
                "printf '%s' \"$DSH_TEST_VALUE\"".to_owned(),
            ],
            &[("DSH_TEST_VALUE".to_owned(), "hello-proxy".to_owned())],
            None,
        )
        .unwrap();
        let outcome = job.wait(Duration::from_secs(5)).unwrap();
        assert_eq!(
            outcome.lines,
            vec![(Stream::Stdout, "hello-proxy".to_owned())]
        );
        assert!(outcome.succeeded());
    }

    #[test]
    fn cancel_stops_a_long_job() {
        let mut job = Job::spawn(
            Path::new("/bin/sh"),
            &["-c".to_owned(), "sleep 30".to_owned()],
            &[],
            None,
        )
        .unwrap();
        let started = Instant::now();
        job.cancel();
        let outcome = job.wait(Duration::from_secs(5)).expect("取消后应当结束");
        assert!(outcome.cancelled);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!outcome.succeeded());
    }

    #[test]
    fn blocking_runner_reports_failure_note() {
        let err = run_blocking(
            Path::new("/bin/sh"),
            &[
                "-c".to_owned(),
                "echo 'npm ERR! 权限不足' >&2; exit 1".to_owned(),
            ],
            &[],
            None,
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(!err.success());
        assert!(err.failure_note().contains("权限不足"));
    }

    #[test]
    fn blocking_runner_times_out() {
        let err = run_blocking(
            Path::new("/bin/sh"),
            &["-c".to_owned(), "sleep 30".to_owned()],
            &[],
            None,
            Duration::from_millis(300),
        );
        assert!(err.is_err(), "超时应当是错误");
    }
}
