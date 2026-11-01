#![cfg(unix)]
// 用假 shell 脚本驱动，unix-only。

//! supervisor 的状态机测试。全部无窗口、无网络 —— 用假的 `dsh` 脚本驱动。

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use dsh_supervisor::{Event, Host, LaunchSpec, ReadyUrl, redact};

static SEQ: AtomicUsize = AtomicUsize::new(0);

fn fake_host(body: &str) -> PathBuf {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("dsh-sup-{}-{n}", std::process::id()));
    fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("fake-dsh.sh");
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write script");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

/// 刚写出的可执行脚本被 exec 时，内核可能还看到 i_writecount>0 而返回 ETXTBSY。
/// 并行测试下这是可复现的，所以统一走这个重试入口。
fn spawn_host(spec: LaunchSpec) -> Host {
    let mut last = None;
    for _ in 0..10 {
        match Host::spawn(spec.clone()) {
            Ok(host) => return host,
            Err(err) if err.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                last = Some(err);
                std::thread::sleep(Duration::from_millis(30));
            }
            Err(err) => panic!("spawn: {err}"),
        }
    }
    panic!("spawn 持续 ETXTBSY：{last:?}");
}

fn spec(script: PathBuf, ready_timeout: Duration) -> LaunchSpec {
    LaunchSpec::new(script)
        .ready_timeout(ready_timeout)
        .stop_grace(Duration::from_millis(1200))
}

/// 轮询直到 `pred` 命中或超时，返回期间收集到的所有事件。
fn drain_until(
    host: &mut Host,
    timeout: Duration,
    mut pred: impl FnMut(&Event) -> bool,
) -> Vec<Event> {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        for ev in host.poll() {
            let hit = pred(&ev);
            seen.push(ev);
            if hit {
                return seen;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    seen
}

fn find_ready(events: &[Event]) -> Option<&ReadyUrl> {
    events.iter().find_map(|e| match e {
        Event::Ready(u) => Some(u),
        _ => None,
    })
}

#[test]
fn ready_url_is_parsed_and_never_leaks_in_debug() {
    let script = fake_host(r#"echo "dsh web: http://127.0.0.1:4444/?token=s3cret"; sleep 30"#);
    let mut host = spawn_host(spec(script, Duration::from_secs(5)));

    let events = drain_until(&mut host, Duration::from_secs(5), |e| {
        matches!(e, Event::Ready(_))
    });
    let url = find_ready(&events).expect("ready event");
    assert!(url.as_str().contains("token=s3cret"), "真实值必须可拿到");
    assert_eq!(url.origin(), "http://127.0.0.1:4444");
    assert!(!format!("{url:?}").contains("s3cret"), "Debug 必须脱敏");
    assert!(!format!("{url}").contains("s3cret"), "Display 必须脱敏");

    // 转发到 UI 的日志行也必须脱敏
    let leaked = events.iter().any(|e| match e {
        Event::Log { line, .. } => line.contains("s3cret"),
        _ => false,
    });
    assert!(!leaked, "日志行泄漏了 token");
}

#[test]
fn delayed_ready_line_is_still_accepted() {
    let script =
        fake_host(r#"sleep 1; echo "dsh web: http://127.0.0.1:4445/?token=abc"; sleep 30"#);
    let mut host = spawn_host(spec(script, Duration::from_secs(5)));
    let started = Instant::now();
    let events = drain_until(&mut host, Duration::from_secs(5), |e| {
        matches!(e, Event::Ready(_))
    });
    assert!(find_ready(&events).is_some(), "延迟打印也应被识别");
    // 不断言精确耗时：CI 机器负载会漂，只验证「晚到的行同样被接受」。
    let _ = started;
}

#[test]
fn missing_ready_line_times_out_with_actionable_message() {
    let script = fake_host("sleep 30");
    let mut host = spawn_host(spec(script, Duration::from_millis(300)));
    let events = drain_until(&mut host, Duration::from_secs(5), |e| {
        matches!(e, Event::Failed(_))
    });
    let msg = events
        .iter()
        .find_map(|e| match e {
            Event::Failed(m) => Some(m.clone()),
            _ => None,
        })
        .expect("timeout failure");
    assert!(msg.contains("就绪"), "错误文案要能指向真正的原因：{msg}");
    assert!(msg.contains("dsh web: "), "文案要引用节点实际要找的行");
}

#[test]
fn crash_after_ready_reports_exit_code() {
    let script = fake_host(r#"echo "dsh web: http://127.0.0.1:4446/?token=x"; exit 1"#);
    let mut host = spawn_host(spec(script, Duration::from_secs(5)));
    let events = drain_until(&mut host, Duration::from_secs(5), |e| {
        matches!(e, Event::Exited { code: Some(1) })
    });
    assert!(find_ready(&events).is_some(), "崩溃前应先看到就绪");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Exited { code: Some(1) })),
        "退出码必须上报：{events:?}"
    );
}

#[test]
fn non_zero_exit_without_ready_is_reported() {
    let script = fake_host(r#"echo "error: profile web failed to initialize" >&2; exit 3"#);
    let mut host = spawn_host(spec(script, Duration::from_secs(30)));
    let events = drain_until(&mut host, Duration::from_secs(5), |e| {
        matches!(e, Event::Exited { .. })
    });
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Exited { code: Some(3) }))
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::Log { line, .. } if line.contains("profile web failed")
        )),
        "stderr 必须转发，否则用户看不到原因"
    );
}

#[test]
fn stop_kills_the_whole_process_group() {
    // 关键回归：M0 实测过，只 kill 直接子进程会留下孙进程。
    let script = fake_host(
        r#"sleep 300 & echo "grandchild=$!" >&2; echo "dsh web: http://127.0.0.1:4447/?token=g"; wait"#,
    );
    let mut host = spawn_host(spec(script, Duration::from_secs(5)));

    let events = drain_until(
        &mut host,
        Duration::from_secs(5),
        |e| matches!(e, Event::Log { line, .. } if line.starts_with("grandchild=")),
    );
    let pid: i32 = events
        .iter()
        .find_map(|e| match e {
            Event::Log { line, .. } => line
                .strip_prefix("grandchild=")
                .and_then(|s| s.trim().parse().ok()),
            _ => None,
        })
        .expect("grandchild pid");
    assert!(alive(pid), "孙进程应已启动（pid {pid}）");

    host.stop();

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && alive(pid) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!alive(pid), "孙进程 {pid} 在 stop() 之后仍然存活");
}

#[test]
fn drop_stops_the_host() {
    let script = fake_host(r#"echo "dsh web: http://127.0.0.1:4448/?token=d"; sleep 300"#);
    let mut host = spawn_host(spec(script, Duration::from_secs(5)));
    let pid = host.pid();
    drain_until(&mut host, Duration::from_secs(5), |e| {
        matches!(e, Event::Ready(_))
    });
    drop(host);
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && alive(pid as i32) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!alive(pid as i32), "Drop 之后宿主进程 {pid} 仍在");
}

#[test]
fn redact_masks_every_token_occurrence() {
    let line = "GET /?token=abc123&x=1 then /?token=zzz\" tail";
    let out = redact(line);
    assert!(!out.contains("abc123"), "{out}");
    assert!(!out.contains("zzz"), "{out}");
    assert!(out.contains("token=<redacted>"));
    assert!(out.contains("&x=1"));
}

fn alive(pid: i32) -> bool {
    // 已退出但未被 reaped 的进程仍占着 /proc，用 stat 的 state 字段区分僵尸。
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // 形如 "123 (cmd) S 1 ..."；状态取最后一个 ')' 之后的第一个字段。
    stat.rsplit(')')
        .next()
        .is_some_and(|rest| !rest.trim_start().starts_with('Z'))
}
