//! 剪贴板里的截图 → 页面里的附件。
//!
//! 背景：WebKitGTK 直到 2.54.1 才允许应用粘贴内存中的图片数据
//! （<https://bugs.webkit.org/show_bug.cgi?id=218519>），Ubuntu 24.04 给的是 2.52。
//! 于是 `paste` 事件里的 `clipboardData` 只有文本，前端没有可收的文件，粘贴静默失败。
//!
//! 壳的做法和其他原生能力一致：**能探测就先探测**。这里读剪贴板位图、编成 PNG，
//! 再把字节以「合成一次 drop」的方式送进页面 —— 前端只需要它本来就有的 drop 处理，
//! 不需要认识桌面壳。
//!
//! 只接管「剪贴板里没有文本」的情形：用户复制图片时常常同时带上文本/HTML，
//! 那种情况下正常粘贴是有意义的，不该被图片抢走。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine as _;

/// 每段注入脚本携带的 base64 长度。整段一次塞进去，某些 WebKitGTK 版本会明显卡顿。
const CHUNK: usize = 256 * 1024;

/// 同一次按键可能同时触发 paste 监听与 keydown 兜底，用哈希 + 时间窗去重。
const DEDUPE: Duration = Duration::from_millis(800);

fn last_delivery() -> &'static Mutex<Option<(u64, Instant)>> {
    static LAST: Mutex<Option<(u64, Instant)>> = Mutex::new(None);
    &LAST
}

fn digest(bytes: &[u8]) -> u64 {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// 读剪贴板里的位图并编码成 PNG；没有图片（或同时有文本）时返回 `None`。
#[must_use]
pub fn read_png() -> Option<Vec<u8>> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    // 有文本就说明这次粘贴多半是「文字 + 图片」的混合剪贴板，交给正常粘贴路径。
    if clipboard
        .get_text()
        .ok()
        .is_some_and(|text| !text.trim().is_empty())
    {
        return None;
    }
    let image = clipboard.get_image().ok()?;
    if image.width == 0 || image.height == 0 || image.bytes.is_empty() {
        return None;
    }
    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, image.width as u32, image.height as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().ok()?;
        writer.write_image_data(&image.bytes).ok()?;
    }
    Some(png)
}

/// 这张图刚送过吗？（同一按键的 paste 与 keydown 两条路径会各问一次）
#[must_use]
pub fn was_just_delivered(png: &[u8]) -> bool {
    let stamp = digest(png);
    let Ok(mut last) = last_delivery().lock() else {
        return false;
    };
    match *last {
        Some((previous, at)) if previous == stamp && at.elapsed() < DEDUPE => true,
        _ => {
            *last = Some((stamp, Instant::now()));
            false
        }
    }
}

fn file_name() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();
    format!("screenshot-{now}.png")
}

/// 把 PNG 编成一串注入脚本：建缓冲 → 分段塞入 → 合成 `File` 并派发 `drop`。
///
/// 走 `drop` 而不是 `paste`：`DragEvent` 的 `dataTransfer` 可以在构造时给进去，
/// 而 `ClipboardEvent` 的 `clipboardData` 由浏览器持有，脚本改不了。
#[must_use]
pub fn paste_scripts(png: &[u8]) -> Vec<String> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(png);
    let mut scripts = Vec::with_capacity(encoded.len() / CHUNK + 2);
    scripts.push("window.__dshPaste = [];".to_owned());
    for chunk in encoded.as_bytes().chunks(CHUNK) {
        // base64 是 ASCII，逐字节转 char 是安全的。
        let piece: String = chunk.iter().map(|byte| char::from(*byte)).collect();
        scripts.push(format!(
            "window.__dshPaste.push({});",
            serde_json::Value::String(piece)
        ));
    }
    scripts.push(drop_script(&file_name()));
    scripts
}

fn drop_script(name: &str) -> String {
    format!(
        r#"(function () {{
  var chunks = window.__dshPaste || [];
  window.__dshPaste = null;
  var binary = atob(chunks.join(''));
  var bytes = new Uint8Array(binary.length);
  for (var index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index);
  var file = new File([bytes], {name}, {{ type: 'image/png' }});
  var transfer = new DataTransfer();
  transfer.items.add(file);
  var target = document.querySelector('[contenteditable="true"]') || document.body;
  var event = new DragEvent('drop', {{ bubbles: true, cancelable: true, dataTransfer: transfer }});
  (target || document).dispatchEvent(event);
  return 'pasted ' + bytes.length;
}})();"#,
        name = serde_json::Value::String(name.to_owned())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 分块注入的每一段必须能原样拼回去——否则页面里 atob 出来的是残缺 PNG。
    #[test]
    fn chunks_reassemble_to_the_original_png() {
        let png: Vec<u8> = (0..CHUNK * 2 + 7)
            .map(|index| (index % 251) as u8)
            .collect();
        let scripts = paste_scripts(&png);
        assert_eq!(scripts.len(), 5, "初始化 + 3 段 + drop");
        assert_eq!(scripts[0], "window.__dshPaste = [];");

        let mut encoded = String::new();
        for script in &scripts[1..scripts.len() - 1] {
            let literal = script
                .strip_prefix("window.__dshPaste.push(")
                .and_then(|rest| rest.strip_suffix(");"))
                .expect("分块脚本形状固定");
            let piece: String = serde_json::from_str(literal).expect("分块应是 JSON 字符串");
            encoded.push_str(&piece);
        }
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("拼回去必须是合法 base64");
        assert_eq!(decoded, png);
    }

    /// 最后一段要带上文件名，并以 `drop` 收尾（前端只认 drop）。
    #[test]
    fn last_script_dispatches_a_drop_with_the_file_name() {
        let scripts = paste_scripts(b"png");
        let last = scripts.last().expect("至少有一段");
        assert!(last.contains("screenshot-"), "文件名前缀：{last}");
        assert!(last.contains("new DragEvent('drop'"), "收尾是 drop：{last}");
        assert!(
            last.contains("items.add(file)"),
            "必须往 DataTransfer 里加文件"
        );
    }
}
