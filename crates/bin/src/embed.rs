//! 把 webview 嵌进 tao 窗口 —— 这是三平台差异最大的一处。
//!
//! Linux 上 wry 需要把 webview 挂到 GTK 容器里；macOS/Windows 直接 build 在窗口上。

use tao::window::Window;
use wry::WebViewBuilder;

#[cfg(target_os = "linux")]
#[must_use]
pub fn build<'a>(builder: WebViewBuilder<'a>, window: &'a Window) -> wry::WebView {
    use tao::platform::unix::WindowExtUnix as _;
    use wry::WebViewBuilderExtUnix as _;
    let vbox = window.default_vbox().expect("tao 的 default_vbox 缺失");
    builder.build_gtk(vbox).expect("创建 webview 失败")
}

#[cfg(not(target_os = "linux"))]
#[must_use]
pub fn build<'a>(builder: WebViewBuilder<'a>, window: &'a Window) -> wry::WebView {
    builder.build(window).expect("创建 webview 失败")
}
