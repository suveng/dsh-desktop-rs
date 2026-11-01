# dsh-desktop-rs

用 Rust 写的 [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness) 桌面客户端，首个目标平台 **Linux**。

- 壳用 `tao` + `wry`（Linux 后端 WebKitGTK），UI 复用 dsh 现有 Web 前端与全部插件生态。
- Rust 只负责：宿主进程监督、窗口、原生菜单/托盘、单实例、深链、通知、更新提示。
- 不实现 dsh 的 RPC 协议，也不 fork dsh。

设计文档：[`docs/architecture.md`](docs/architecture.md)

## 当前状态

设计稿阶段，未开始编码。下一步是 M0：验证 WebKitGTK 能否正常渲染 dsh 前端（见文档 §11）。
