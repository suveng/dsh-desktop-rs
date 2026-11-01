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

  // 危险点：前端插件以「protocolVersion === 1」判定桌面形态，然后**直接调用**这些成员。
  // 少一个方法就会让插件在 activate 阶段抛错，整个 web boot 失败。
  // 所以这里必须给出完整形状，不支持的能力用「安全空实现 + 明确拒绝」表达。
  const unsupported = (what) => () => Promise.reject(new Error('dsh-desktop: 不支持 ' + what));
  const noop = () => {};

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
      status: () => Promise.resolve({ phase: 'idle' }),
      open: () => Promise.resolve(),
      subscribe: () => noop,
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
  // R2：WebKitGTK 的 File 没有真实路径。返回空串让前端退回「上传字节」。
  define('__DSH_HOST_PATHS__', { pathFor: () => '' });

  document.documentElement.setAttribute('data-dsh-desktop-shim', '1');
})();
