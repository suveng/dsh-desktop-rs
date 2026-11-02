/*
 * 桌面能力 shim（architecture.md D4）。
 *
 * 目标不是复刻 Electron 的 preload，而是让前端那些 `protocolVersion === 1` 探测
 * 命中，并且拿到它真正会用的几样东西：设备信息、原生目录选择。
 *
 * 约定：页面 → host 用 window.ipc.postMessage(JSON)，host → 页面用
 * `window.__dshShimResolve(id, ok, value)` 回填。
 */
(function () {
  // B 层证据：壳内的 WebSocket 是否真的收到流式帧。
  window.__dshWs = { opens: 0, msgs: 0, types: {} };
  (function () {
    var WS = window.WebSocket;
    function Hooked(url, protocols) {
      var ws = protocols === undefined ? new WS(url) : new WS(url, protocols);
      window.__dshWs.opens += 1;
      ws.addEventListener('message', function (e) {
        window.__dshWs.msgs += 1;
        try {
          var t = (JSON.parse(e.data) || {}).type || 'unknown';
          window.__dshWs.types[t] = (window.__dshWs.types[t] || 0) + 1;
        } catch (_) {}
      });
      return ws;
    }
    Hooked.prototype = WS.prototype;
    Hooked.CONNECTING = WS.CONNECTING; Hooked.OPEN = WS.OPEN;
    Hooked.CLOSING = WS.CLOSING; Hooked.CLOSED = WS.CLOSED;
    window.WebSocket = Hooked;
  })();

  const pending = new Map();
  let seq = 0;

  const call = (kind, payload) =>
    new Promise((resolve, reject) => {
      const id = ++seq;
      pending.set(id, { resolve, reject });
      window.ipc.postMessage(JSON.stringify(Object.assign({ kind: kind, id: id }, payload || {})));
    });

  window.__dshShimResolve = function (id, ok, value) {
    const entry = pending.get(id);
    if (!entry) return;
    pending.delete(id);
    if (ok) entry.resolve(value);
    else entry.reject(new Error(String(value)));
  };

  const picker = { pick: () => call('pick-directory') };

  /*
   * 截图粘贴。WebKitGTK 直到 2.54.1 才允许粘贴内存中的图片（webkit bug 218519），
   * Ubuntu 24.04 的 2.52 里 paste 事件的 clipboardData 根本没有图片项。两条路：
   *   ① HTML 片段里带 data:image/ 的图（WebKit ≥2.54 与 Chromium 都是这样）→ 本地解码成 File；
   *   ② 剪贴板只有位图、没有文本 → 请壳读位图并送回页面。
   * 两者都收敛到同一次「合成 drop」：前端只需要它本来就有的 drop 处理。
   */
  const dropFile = function (file) {
    try {
      const transfer = new DataTransfer();
      transfer.items.add(file);
      const target = document.querySelector('[contenteditable="true"]') || document.body;
      const event = new DragEvent('drop', { bubbles: true, cancelable: true, dataTransfer: transfer });
      (target || document).dispatchEvent(event);
      return true;
    } catch (_error) {
      return false;
    }
  };

  const fileFromDataUrl = function (html) {
    if (!html || html.indexOf('data:image/') === -1) return null;
    try {
      const doc = new DOMParser().parseFromString(html, 'text/html');
      const image = doc.querySelector('img[src^="data:image/"]');
      if (!image) return null;
      const src = image.getAttribute('src') || '';
      const comma = src.indexOf(',');
      if (comma === -1) return null;
      const type = (src.slice(5, comma).split(';')[0] || 'image/png').toLowerCase();
      const binary = atob(src.slice(comma + 1));
      const bytes = new Uint8Array(binary.length);
      for (let index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index);
      const stamp = new Date().toISOString().replace(/[:.]/g, '-');
      return new File([bytes], 'pasted-' + stamp + '.' + (type.split('/')[1] || 'png'), { type: type });
    } catch (_error) {
      return null;
    }
  };

  // 壳会自己判断「有没有图片、有没有文本」；纯文本粘贴时它什么都不做。
  const askClipboardImage = function () {
    call('clipboard-image').catch(noop);
  };

  document.addEventListener('paste', function (event) {
    const data = event.clipboardData;
    if (!data) return;
    const items = data.items || [];
    for (let index = 0; index < items.length; index += 1) {
      const item = items[index];
      // 前端能自己收下的（真文件、真位图）就不要插手。
      if (item.kind === 'file' && item.getAsFile && item.getAsFile()) return;
    }
    let html = '';
    let text = '';
    try { html = data.getData('text/html') || ''; } catch (_error) { html = ''; }
    try { text = data.getData('text/plain') || ''; } catch (_error) { text = ''; }
    const file = fileFromDataUrl(html);
    if (file) {
      event.preventDefault();
      event.stopPropagation();
      dropFile(file);
      return;
    }
    if (text === '' && html === '') askClipboardImage();
  }, true);

  // 有的 WebKitGTK 版本对「只有位图」的剪贴板连 paste 事件都不发，用按键兜底。
  document.addEventListener('keydown', function (event) {
    if (!(event.ctrlKey || event.metaKey) || event.altKey) return;
    if (event.key !== 'v' && event.key !== 'V') return;
    askClipboardImage();
  }, true);

  // 危险点：前端插件以「protocolVersion === 1」判定桌面形态，然后**直接调用**这些成员。
  // 少一个方法就会让插件在 activate 阶段抛错，整个 web boot 失败。
  // 所以这里必须给出完整形状，不支持的能力用「安全空实现 + 明确拒绝」表达。
  const unsupported = (what) => () => Promise.reject(new Error('dsh-desktop: 不支持 ' + what));
  const noop = () => {};

  // 更新状态由壳推过来：前端订阅的是这里注册的监听者。
  const updateListeners = new Set();
  window.__dshShimEmitUpdate = function (state) {
    updateListeners.forEach(function (listener) {
      try {
        listener(state);
      } catch (_error) {
        /* 单个监听者报错不该影响其它订阅者 */
      }
    });
  };

  const desktop = {
    protocolVersion: 1,
    deviceInfo: () => call('device-info'),
    directoryPicker: picker,
    fileActions: {
      save: unsupported('fileActions.save'),
      readClipboardImage: unsupported('fileActions.readClipboardImage'),
    },
    browser: { open: unsupported('browser.open'), close: noop },
    fileDrag: {
      prepare: () => Promise.resolve(undefined),
      cancel: noop,
      start: () => Promise.resolve(false),
      release: noop,
    },
    keyboard: { closeWindow: () => Promise.resolve(), subscribe: () => noop },
    shortcuts: {
      get: () => Promise.resolve({}),
      edit: unsupported('shortcuts.edit'),
      recording: () => Promise.resolve(),
      subscribe: () => noop,
    },
    updates: {
      status: () => call('update-status'),
      open: () => call('update-open'),
      subscribe: function (listener) {
        updateListeners.add(listener);
        return function () {
          updateListeners.delete(listener);
        };
      },
    },
  };

  const define = (name, value) => {
    try {
      Object.defineProperty(window, name, { value: value, writable: false, configurable: true });
    } catch (_error) {
      window[name] = value;
    }
  };

  define('dshDesktop', desktop);
  define('__DSH_DIRECTORY_PICKER__', picker);
  // 原生 drop 的真实路径由壳按文件名回填（wry 的拖拽事件带完整路径，WebKitGTK 的 File 没有）。
  define('__DSH_HOST_PATHS__', {
    pathFor: function (file) {
      var name = file && file.name ? String(file.name) : '';
      if (!name) return Promise.resolve('');
      return call('host-path', { name: name });
    },
  });

  document.documentElement.setAttribute('data-dsh-desktop-shim', '1');
})();
