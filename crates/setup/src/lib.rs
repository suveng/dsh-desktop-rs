//! 安装引导的全部纯逻辑：环境检测、安装任务、node / dsh 安装器、profile 管理、配置。
//!
//! 这个 crate **不依赖 GUI**：`crates/bin` 只负责把这里的事件画到界面上。
//! 好处是这些逻辑可以用假 `npm` / 假 `node` / 临时目录直接测，不需要开窗口、不联网。

pub mod envcheck;
pub mod install;
pub mod node;
pub mod profiles;
pub mod settings;
pub mod shellenv;
pub mod stamp;
pub mod version;

pub use envcheck::{DshRuntime, NodeRuntime, Source};
pub use install::{Job, JobEvent, JobOutcome, Stream};
pub use settings::{InstallScope, Proxy, Settings};
pub use version::Version;
