//! 单实例：loopback socket + `0600` 发现文件（`architecture.md` D5）。
//!
//! 不引 D-Bus、不依赖窗口管理器 —— 后者在本机实测根本没有（见
//! `docs/spike-webkitgtk.md` §4-2），所以任何走 EWMH/WM 的方案都不可靠。

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::runtime_dir;

/// 第二个实例想让第一个实例做的事。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// 把已有窗口提到前面。
    Focus,
    /// 处理一个深链（`dsh://…`）。
    OpenUrl(String),
}

impl Command {
    fn encode(&self) -> String {
        match self {
            Self::Focus => "focus".to_owned(),
            Self::OpenUrl(url) => format!("open-url {url}"),
        }
    }

    fn decode(body: &str) -> Option<Self> {
        let body = body.trim();
        if body == "focus" {
            return Some(Self::Focus);
        }
        body.strip_prefix("open-url ")
            .map(|url| Self::OpenUrl(url.to_owned()))
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Discovery {
    port: u16,
    token: String,
    pid: u32,
}

/// `claim()` 的结果。
pub enum Claim {
    /// 我们就是唯一实例，拿到监听器。
    Primary(Instance),
    /// 已经有实例在跑；调用方应 `send()` 自己的请求然后退出。
    Secondary,
}

/// 主实例持有的监听器。
pub struct Instance {
    listener: TcpListener,
    path: PathBuf,
    token: String,
}

impl Instance {
    /// 尝试成为主实例。
    ///
    /// 步骤：拿 `lock` 文件（`create_new`，天然互斥）→ 若已存在且 pid 活着 → `Secondary`；
    /// 否则清理遗留的 `lock` 与发现文件，绑定临时端口并写回发现文件。
    pub fn claim() -> std::io::Result<Claim> {
        let dir = runtime_dir();
        fs::create_dir_all(&dir)?;
        restrict(&dir, 0o700)?;
        let lock = dir.join("lock");
        let path = dir.join("instance.json");

        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
        {
            Ok(mut file) => {
                let _ = write!(file, "{}", std::process::id());
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                if owner_alive(&lock) {
                    return Ok(Claim::Secondary);
                }
                // 上次异常退出留下的锁：清掉再抢一次。
                let _ = fs::remove_file(&lock);
                let _ = fs::remove_file(&path);
                return Self::claim();
            }
            Err(err) => return Err(err),
        }

        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
        let port = listener.local_addr()?.port();
        let token = random_token()?;
        let discovery = Discovery {
            port,
            token: token.clone(),
            pid: std::process::id(),
        };
        write_discovery(&path, &discovery)?;

        Ok(Claim::Primary(Self {
            listener,
            path,
            token,
        }))
    }

    /// 在后台线程里接收命令。连接只允许来自 loopback 且必须带对 token。
    #[must_use]
    pub fn commands(&self) -> Receiver<Command> {
        let (tx, rx) = channel();
        let listener = self.listener.try_clone().expect("复制监听器");
        let token = self.token.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Ok(reader_stream) = stream.try_clone() else {
                    continue;
                };
                let mut line = String::new();
                if BufReader::new(reader_stream).read_line(&mut line).is_err() {
                    continue;
                }
                let ok = verify(&token, &line, &tx);
                let _ = stream.write_all(if ok { b"ok\n" } else { b"denied\n" });
            }
        });
        rx
    }

    /// 退出时删掉发现文件与锁。
    pub fn release(&self) {
        let _ = fs::remove_file(&self.path);
        if let Some(dir) = self.path.parent() {
            let _ = fs::remove_file(dir.join("lock"));
        }
    }
}

/// 第二个实例把请求发给主实例。
pub fn send(command: &Command) -> std::io::Result<()> {
    let path = runtime_dir().join("instance.json");
    let raw = fs::read_to_string(path)?;
    let discovery: Discovery = serde_json::from_str(&raw)?;
    let mut stream = TcpStream::connect_timeout(
        &SocketAddr::from((Ipv4Addr::LOCALHOST, discovery.port)),
        Duration::from_secs(2),
    )?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    writeln!(stream, "{} {}", discovery.token, command.encode())?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    if reply.trim() == "ok" {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("主实例拒绝请求：{reply}")))
    }
}

fn verify(token: &str, line: &str, tx: &std::sync::mpsc::Sender<Command>) -> bool {
    let Some((sent, rest)) = line.split_once(' ') else {
        return false;
    };
    if sent != token {
        return false;
    }
    match Command::decode(rest) {
        Some(command) => tx.send(command).is_ok(),
        None => false,
    }
}

fn owner_alive(lock: &PathBuf) -> bool {
    let Ok(pid) = fs::read_to_string(lock).and_then(|s| {
        s.trim()
            .parse::<u32>()
            .map_err(|e| std::io::Error::other(e.to_string()))
    }) else {
        return false;
    };
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

fn random_token() -> std::io::Result<String> {
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn write_discovery(path: &PathBuf, discovery: &Discovery) -> std::io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec(discovery)?)?;
    restrict(&tmp, 0o600)?;
    fs::rename(&tmp, path)
}

#[cfg(unix)]
fn restrict(path: &std::path::Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn restrict(_path: &std::path::Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    /// 这些用例改的是进程级环境变量，必须串行跑。
    static ENV: Mutex<()> = Mutex::new(());

    fn sandbox(name: &str) -> (MutexGuard<'static, ()>, PathBuf) {
        let guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("dsh-inst-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // SAFETY: 持锁期间没有别的用例读这些变量。
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", &dir) };
        (guard, dir)
    }

    #[test]
    fn second_claim_is_secondary_and_can_send_commands() {
        let (_env, _dir) = sandbox("second");
        let Claim::Primary(instance) = Instance::claim().unwrap() else {
            panic!("第一次应当拿到主实例");
        };
        let commands = instance.commands();

        assert!(matches!(Instance::claim().unwrap(), Claim::Secondary));

        send(&Command::Focus).unwrap();
        assert_eq!(
            commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            Command::Focus
        );

        send(&Command::OpenUrl("dsh://session/abc".into())).unwrap();
        assert_eq!(
            commands.recv_timeout(Duration::from_secs(2)).unwrap(),
            Command::OpenUrl("dsh://session/abc".into())
        );

        instance.release();
    }

    #[test]
    fn stale_lock_from_dead_process_is_reclaimed() {
        let (_env, dir) = sandbox("stale");
        // 一个几乎不可能存在的 pid。
        fs::write(dir.join("lock"), "4294967290").unwrap();
        assert!(matches!(Instance::claim().unwrap(), Claim::Primary(_)));
    }

    #[test]
    fn discovery_file_is_private() {
        let (_env, _dir) = sandbox("perm");
        let Claim::Primary(instance) = Instance::claim().unwrap() else {
            panic!("应当拿到主实例");
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(runtime_dir().join("instance.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        instance.release();
    }

    #[test]
    fn wrong_token_is_rejected() {
        let (_env, _dir) = sandbox("token");
        let Claim::Primary(instance) = Instance::claim().unwrap() else {
            panic!("应当拿到主实例");
        };
        let commands = instance.commands();
        let discovery: Discovery =
            serde_json::from_str(&fs::read_to_string(runtime_dir().join("instance.json")).unwrap())
                .unwrap();

        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, discovery.port)).expect("连接");
        writeln!(stream, "not-the-token focus").unwrap();
        let mut reply = String::new();
        BufReader::new(stream).read_line(&mut reply).unwrap();
        assert_eq!(reply.trim(), "denied");
        assert!(commands.recv_timeout(Duration::from_millis(200)).is_err());
    }
}
