import { invoke, Channel } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

interface RemoteSession {
  id: string;
  type: 'vnc' | 'rdp' | 'spice';
  canvas: HTMLCanvasElement;
  ctx: CanvasRenderingContext2D;
  wrapper: HTMLElement;
  width: number;
  height: number;
  /** RDP 用：'fit-window' 或 'native'。其它协议为 undefined。 */
  displayMode?: 'fit-window' | 'native';
  /** SPICE 用：'client'（绝对定位）或 'server'（相对位移）。 */
  spiceMouseMode?: 'client' | 'server';
  /** SPICE 用：多 surface 时当前活跃的 surface id。 */
  spiceActiveSurfaceId?: number;
}

const sessions = new Map<string, RemoteSession>();
const pendingResolutions = new Map<string, { width: number; height: number }>();
// 防止同一 session 的帧堆积：上一帧还没画完就丢弃新帧
const drawingSessions = new Set<string>();

const MOUSE_INTERVAL_MS = 16;

/** 每个 RDP 会话的输入事件队列。 */
const rdpInputQueues = new Map<string, any[]>();

/** 每个 SPICE 会话的输入事件队列。 */
const spiceInputQueues = new Map<string, any[]>();
/** 每个 SPICE 会话按下的 scancode 集合，blur 时逐个释放。 */
const pressedSpiceKeys = new Map<string, Set<number>>();

function ensureSpiceFlush(sessionId: string): void {
  if (spiceInputQueues.has(sessionId)) return;
  spiceInputQueues.set(sessionId, []);
  setInterval(() => {
    const queue = spiceInputQueues.get(sessionId);
    if (!queue || queue.length === 0) return;
    const events = queue.splice(0, queue.length);
    invoke('spice_send_input', { sessionId, events }).catch((err) => {
      console.error('[SPICE] input 失败:', err);
    });
  }, 16);
}

function pushSpiceInput(sessionId: string, event: any): void {
  ensureSpiceFlush(sessionId);
  const queue = spiceInputQueues.get(sessionId);
  if (queue) queue.push(event);
}

function buttonsToMask(buttons: number): number {
  let m = 0;
  if (buttons & 1) m |= 1;   // left
  if (buttons & 2) m |= 4;   // middle
  if (buttons & 4) m |= 2;   // right
  return m;
}

/** 每个 VNC 会话的输入事件队列。 */
const vncInputQueues = new Map<string, any[]>();

function ensureVncFlush(sessionId: string): void {
  if (vncInputQueues.has(sessionId)) return;
  vncInputQueues.set(sessionId, []);
  setInterval(() => {
    const queue = vncInputQueues.get(sessionId);
    if (!queue || queue.length === 0) return;
    const events = queue.splice(0, queue.length);
    invoke('vnc_send_input', { sessionId, events }).catch((err) => {
      console.error('[VNC] input 失败:', err);
    });
  }, 16);
}

function pushVncInput(sessionId: string, event: any): void {
  console.log('[VNC] push', event);
  ensureVncFlush(sessionId);
  const queue = vncInputQueues.get(sessionId);
  if (queue) queue.push(event);
}

/** 启动批量发送定时器（每个会话一个）。 */
function ensureRdpFlush(sessionId: string): void {
  if (rdpInputQueues.has(sessionId)) return;
  rdpInputQueues.set(sessionId, []);
  setInterval(() => {
    const queue = rdpInputQueues.get(sessionId);
    if (!queue || queue.length === 0) return;
    const events = queue.splice(0, queue.length);
    invoke('rdp_send_input_batch', { sessionId, events }).catch(() => {});
  }, 16);
}

/** 把事件推入队列。 */
function pushRdpInput(sessionId: string, event: any): void {
  ensureRdpFlush(sessionId);
  const queue = rdpInputQueues.get(sessionId);
  if (queue) queue.push(event);
}

/**
 * 为一个会话创建帧通道。
 *
 * 切标签时用它重新挂载：detach 后旧 Channel 作废，
 * 切回来必须新建一个并再次 attach 给后端。
 */
export function makeFrameChannel(sessionId: string): Channel<ArrayBuffer> {
  const ch = new Channel<ArrayBuffer>();
  ch.onmessage = (raw) => handleBinaryFrame(sessionId, raw);
  return ch;
}

/** 查询某会话的 displayMode。main.ts 的窗口 resize 联动需要它。 */
export function getRemoteDisplayMode(
  sessionId: string,
): 'fit-window' | 'native' | undefined {
  return sessions.get(sessionId)?.displayMode;
}

export async function connectRemote(
  type: 'vnc' | 'rdp' | 'spice',
  host: string,
  port: number,
  username: string,
  password: string,
  wrapper: HTMLElement,
): Promise<string> {
  await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));

  const canvas = document.createElement('canvas');

  // RDP 用 wrapper 的实际尺寸作为初始分辨率，保证 RDP 桌面 = 窗口尺寸，
  // 画面不会被裁切或留黑边。VNC/SPICE 仍用 1280x720 兜底。
  const rect = wrapper.getBoundingClientRect();
  const initW = type === 'rdp'
    ? Math.max(640, Math.min(7680, Math.floor(rect.width) || 1280))
    : 1280;
  const initH = type === 'rdp'
    ? Math.max(480, Math.min(4320, Math.floor(rect.height) || 720))
    : 720;

  canvas.width = initW;
  canvas.height = initH;
  canvas.style.width = '100%';
  canvas.style.height = '100%';
  canvas.style.background = '#000';
  canvas.style.display = 'block';
  canvas.style.outline = 'none';
  canvas.style.cursor = 'default';
  canvas.tabIndex = 0;

  wrapper.textContent = '';
  wrapper.appendChild(canvas);

  const ctx = canvas.getContext('2d', { alpha: false });
  if (!ctx) throw new Error('无法获取 canvas 2D 上下文');

  let sessionId: string;
  let displayMode: 'fit-window' | 'native' | undefined;

  if (type === 'vnc') {
    const frameChannel = new Channel<ArrayBuffer>();
    frameChannel.onmessage = (raw) => {
      handleBinaryFrame(sessionId, raw);
    };
    sessionId = await invoke<string>('vnc_connect', {
      host, port, password,
      frameChannel,
    });
    
    // 监听 VNC 状态，失败时清理 canvas
    listen<{ sessionId: string; state: string; message: string | null; errorKind: string | null }>(
      `vnc-state-${sessionId}`,
      (e) => {
        const { state, message } = e.payload;
        if (state === 'failed') {
          console.error('[VNC] 连接失败:', message);
          // 清理 session 和 canvas
          const s = sessions.get(sessionId);
          if (s) {
            s.canvas.remove();
            sessions.delete(sessionId);
          }
          void invoke('vnc_close', { sessionId }).catch(() => {});
        } else if (state === 'active') {
          console.log('[VNC] 已连接');
        }
      },
    ).catch(() => {});
    
    listen<{ sessionId: string; text: string }>(
      `vnc-clipboard-${sessionId}`,
      (e) => {
        void navigator.clipboard.writeText(e.payload.text).catch(() => {});
      },
    ).catch(() => {});
  } else if (type === 'rdp') {
    const frameChannel = new Channel<ArrayBuffer>();
    sessionId = await invoke<string>('rdp_connect', {
      host, port, username, password,
      width: initW, height: initH,
      displayMode: 'native',
      colorDepth: 32,
      certificatePolicy: 'prompt',
      clipboardMode: 'text-and-files',
      frameChannel,
    });
    frameChannel.onmessage = (raw) => handleBinaryFrame(sessionId, raw);
    displayMode = 'native';
  } else if (type === 'spice') {
    console.log('[SPICE] === SPICE branch entered ===', { host, port });
    const frameChannel = new Channel<ArrayBuffer>();
    let realSessionId = '';
    frameChannel.onmessage = (raw) => {
      if (!realSessionId) return;
      handleBinaryFrame(realSessionId, raw);
    };
    try {
      sessionId = await invoke<string>('spice_connect', {
        host, port,
        password: password || null,
        frameChannel,
      });
      realSessionId = sessionId;
      console.log('[SPICE] spice_connect returned:', sessionId);
    } catch (e) {
      console.error('[SPICE] spice_connect FAILED:', e);
      throw e;
    }

    listen<{ sessionId: string; state: string; message: string | null; errorKind: string | null }>(
      `spice-state-${sessionId}`,
      (e) => {
        console.log('[SPICE] state event:', e.payload);
        const { state, message } = e.payload;
        if (state === 'failed') {
          console.error('[SPICE] 连接失败:', message);
        } else if (state === 'active') {
          console.log('[SPICE] 已连接');
        }
      },
    ).then(() => console.log('[SPICE] listen registered for', sessionId))
     .catch((err) => console.error('[SPICE] listen failed:', err));
  } else {
    const _exhaustive: never = type;
    throw new Error(`未知远程桌面类型: ${_exhaustive}`);
  }

  const pending = pendingResolutions.get(sessionId);
  if (pending) {
    canvas.width = pending.width;
    canvas.height = pending.height;
    pendingResolutions.delete(sessionId);
  }

  sessions.set(sessionId, {
    id: sessionId, type, canvas, ctx, wrapper,
    width: canvas.width, height: canvas.height,
    displayMode,
  });

  bindInput(sessionId, type, canvas);
  canvas.focus();
  return sessionId;
}

async function handleBinaryFrame(
  sessionId: string,
  raw: ArrayBuffer | Uint8Array,
): Promise<void> {
  const s = sessions.get(sessionId);
  if (!s) return;
  // 上一帧还没画完就丢弃新帧
  if (drawingSessions.has(sessionId)) return;
  drawingSessions.add(sessionId);
  try {
    const bytes = raw instanceof Uint8Array ? raw : new Uint8Array(raw);
    if (bytes.byteLength < 12) return;

    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const x = view.getUint16(0, true);
    const y = view.getUint16(2, true);
    const w = view.getUint16(4, true);
    const h = view.getUint16(6, true);
    const jpegLen = view.getUint32(8, true);
    if (bytes.byteLength < 12 + jpegLen) return;

    // 只有全屏帧（x=0, y=0 且尺寸较大）才同步 canvas 尺寸。
    // 小脏矩形（如 32x32）不能改 canvas，否则画面会被裁切。
    if (x === 0 && y === 0 && w >= 64 && h >= 64) {
      if (s.canvas.width !== w || s.canvas.height !== h) {
        s.canvas.width = w;
        s.canvas.height = h;
        s.width = w;
        s.height = h;
      }
    }

    const jpegBytes = new Uint8Array(bytes.subarray(12, 12 + jpegLen));
    const blob = new Blob([jpegBytes], { type: 'image/jpeg' });
    const bitmap = await createImageBitmap(blob);
    s.ctx.drawImage(bitmap, x, y);
    bitmap.close();

    const wAny = window as any;
    wAny.__rdCount = (wAny.__rdCount ?? 0) + 1;
    if (wAny.__rdCount <= 30) {
      console.log(
        `[RD] #${wAny.__rdCount} region=${w}x${h}@(${x},${y}) jpeg=${jpegLen}B`,
      );
    }
  } finally {
    drawingSessions.delete(sessionId);
  }
}

function bindInput(sessionId: string, type: 'vnc' | 'rdp' | 'spice', canvas: HTMLCanvasElement): void {
  canvas.tabIndex = 0;
  canvas.focus();

  canvas.addEventListener('mousedown', () => canvas.focus());

  // ===== 键盘 =====
  canvas.addEventListener('keydown', (e) => {
    e.preventDefault();
    if (type === 'vnc') {
      const keysym = keyToKeysym(e);
      if (keysym === 0) return;
      pushVncInput(sessionId, { type: 'key', keysym, pressed: true });
    } else if (type === 'rdp') {
      const sc = keyToScancode(e);
      if (sc === 0) return;
      pushRdpInput(sessionId, { kind: 'key', code: sc, down: true, extended: isExtendedKey(e) });
    } else if (type === 'spice') {
      const sc = keyToScancode(e);
      if (sc === 0) return;
      let set = pressedSpiceKeys.get(sessionId);
      if (!set) { set = new Set(); pressedSpiceKeys.set(sessionId, set); }
      set.add(sc);
      pushSpiceInput(sessionId, { type: 'key', scancode: sc, pressed: true });
    }
  });

  canvas.addEventListener('keyup', (e) => {
    if (type === 'vnc') {
      const keysym = keyToKeysym(e);
      if (keysym === 0) return;
      pushVncInput(sessionId, { type: 'key', keysym, pressed: false });
    } else if (type === 'rdp') {
      const sc = keyToScancode(e);
      if (sc === 0) return;
      pushRdpInput(sessionId, { kind: 'key', code: sc, down: false, extended: isExtendedKey(e) });
    } else if (type === 'spice') {
      const sc = keyToScancode(e);
      if (sc === 0) return;
      pressedSpiceKeys.get(sessionId)?.delete(sc);
      pushSpiceInput(sessionId, { type: 'key', scancode: sc, pressed: false });
    }
  });

  canvas.addEventListener('contextmenu', (e) => {
    e.preventDefault();
    e.stopPropagation();
  });

  // ===== 鼠标 =====
  canvas.addEventListener('mousedown', (e) => {
    if (type === 'vnc') sendPointerVnc(sessionId, canvas, e);
    else if (type === 'rdp') sendPointerRdp(sessionId, canvas, e, true);
    else if (type === 'spice') sendPointerSpice(sessionId, canvas, e, true);
  });

  canvas.addEventListener('mouseup', (e) => {
    if (type === 'vnc') sendPointerVnc(sessionId, canvas, e);
    else if (type === 'rdp') sendPointerRdp(sessionId, canvas, e, false);
    else if (type === 'spice') sendPointerSpice(sessionId, canvas, e, false);
  });

  let lastMouseSend = 0;
  canvas.addEventListener('mousemove', (e) => {
    const now = performance.now();
    if (now - lastMouseSend < MOUSE_INTERVAL_MS) return;
    lastMouseSend = now;
    if (type === 'vnc') sendPointerVnc(sessionId, canvas, e);
    else if (type === 'rdp') sendPointerRdpMove(sessionId, canvas, e);
    else if (type === 'spice') sendPointerSpiceMove(sessionId, canvas, e);
  });

  // ===== 滚轮（仅 RDP）=====
  canvas.addEventListener('wheel', (e) => {
    if (type !== 'rdp') return;
    e.preventDefault();
    const rect = canvas.getBoundingClientRect();
    const x = Math.round(((e.clientX - rect.left) / rect.width) * canvas.width);
    const y = Math.round(((e.clientY - rect.top) / rect.height) * canvas.height);
    pushRdpInput(sessionId, {
      kind: 'mouse-wheel',
      deltaX: e.deltaX,
      deltaY: e.deltaY,
      x, y,
    });
  }, { passive: false });

  // ===== 剪贴板粘贴（Ctrl+V 走这里）=====
    canvas.addEventListener('paste', async (e) => {
    const text = e.clipboardData?.getData('text');
    if (!text) return;
    if (type === 'rdp') {
      e.preventDefault();
      await invoke('rdp_set_clipboard_text', { sessionId, text }).catch(() => {});
    } else if (type === 'vnc') {
      e.preventDefault();
      await invoke('vnc_set_clipboard_text', { sessionId, text }).catch(() => {});
    }
    // SPICE：capsaicin 未实现 Agent channel，暂不支持剪贴板
  });

  // ===== 失焦时释放所有按键 =====
    canvas.addEventListener('blur', () => {
    if (type === 'rdp') {
      pushRdpInput(sessionId, { kind: 'release-all-keys' });
    } else if (type === 'vnc') {
      pushVncInput(sessionId, { type: 'release-all-keys' });
    } else if (type === 'spice') {
      const set = pressedSpiceKeys.get(sessionId);
      if (set) {
        for (const sc of set) {
          pushSpiceInput(sessionId, { type: 'key', scancode: sc, pressed: false });
        }
        set.clear();
      }
    }
  });
}

function sendPointerVnc(sessionId: string, canvas: HTMLCanvasElement, e: MouseEvent): void {
  const rect = canvas.getBoundingClientRect();
  const x = Math.round(((e.clientX - rect.left) / rect.width) * canvas.width);
  const y = Math.round(((e.clientY - rect.top) / rect.height) * canvas.height);
  let rfbButtons = 0;
  if (e.buttons & 1) rfbButtons |= 1;   // left
  if (e.buttons & 2) rfbButtons |= 4;   // middle
  if (e.buttons & 4) rfbButtons |= 2;   // right
  pushVncInput(sessionId, { type: 'pointer', x, y, buttonMask: rfbButtons });
}

function sendPointerRdp(
  sessionId: string,
  canvas: HTMLCanvasElement,
  e: MouseEvent,
  down: boolean,
): void {
  const rect = canvas.getBoundingClientRect();
  const x = Math.round(((e.clientX - rect.left) / rect.width) * canvas.width);
  const y = Math.round(((e.clientY - rect.top) / rect.height) * canvas.height);
  const button = e.button === 0 ? 'left' : e.button === 2 ? 'right' : 'middle';
  pushRdpInput(sessionId, { kind: 'pointer', x, y, button, down });
}

function sendPointerRdpMove(
  sessionId: string,
  canvas: HTMLCanvasElement,
  e: MouseEvent,
): void {
  const rect = canvas.getBoundingClientRect();
  const x = Math.round(((e.clientX - rect.left) / rect.width) * canvas.width);
  const y = Math.round(((e.clientY - rect.top) / rect.height) * canvas.height);
  pushRdpInput(sessionId, { kind: 'pointer', x, y, button: 'none', down: false });
}

function sendPointerSpice(sessionId: string, canvas: HTMLCanvasElement, e: MouseEvent, down: boolean): void {
  const rect = canvas.getBoundingClientRect();
  const x = Math.round(((e.clientX - rect.left) / rect.width) * canvas.width);
  const y = Math.round(((e.clientY - rect.top) / rect.height) * canvas.height);
  const button = e.button === 0 ? 1 : e.button === 2 ? 3 : 2;
  const mask = buttonsToMask(e.buttons);
  if (down) {
    pushSpiceInput(sessionId, { type: 'mouse-press', button, buttons: mask });
  } else {
    pushSpiceInput(sessionId, { type: 'mouse-release', button, buttons: mask });
  }
  // 同时更新位置，避免按压期间指针飘移
  pushSpiceInput(sessionId, { type: 'pointer-abs', x, y, buttons: mask });
}

function sendPointerSpiceMove(sessionId: string, canvas: HTMLCanvasElement, e: MouseEvent): void {
  const s = sessions.get(sessionId);
  const mode = s?.spiceMouseMode ?? 'client';
  const mask = buttonsToMask(e.buttons);
  if (mode === 'server') {
    pushSpiceInput(sessionId, {
      type: 'pointer-rel',
      dx: e.movementX,
      dy: e.movementY,
      buttons: mask,
    });
  } else {
    const rect = canvas.getBoundingClientRect();
    const x = Math.round(((e.clientX - rect.left) / rect.width) * canvas.width);
    const y = Math.round(((e.clientY - rect.top) / rect.height) * canvas.height);
    pushSpiceInput(sessionId, { type: 'pointer-abs', x, y, buttons: mask });
  }
}

function keyToScancode(e: KeyboardEvent): number {
  const map: Record<string, number> = {
    Escape: 0x01, Digit1: 0x02, Digit2: 0x03, Digit3: 0x04, Digit4: 0x05,
    Digit5: 0x06, Digit6: 0x07, Digit7: 0x08, Digit8: 0x09, Digit9: 0x0A,
    Digit0: 0x0B, Minus: 0x0C, Equal: 0x0D, Backspace: 0x0E, Tab: 0x0F,
    KeyQ: 0x10, KeyW: 0x11, KeyE: 0x12, KeyR: 0x13, KeyT: 0x14,
    KeyY: 0x15, KeyU: 0x16, KeyI: 0x17, KeyO: 0x18, KeyP: 0x19,
    BracketLeft: 0x1A, BracketRight: 0x1B, Enter: 0x1C, ControlLeft: 0x1D,
    KeyA: 0x1E, KeyS: 0x1F, KeyD: 0x20, KeyF: 0x21, KeyG: 0x22,
    KeyH: 0x23, KeyJ: 0x24, KeyK: 0x25, KeyL: 0x26,
    Semicolon: 0x27, Quote: 0x28, Backquote: 0x29, ShiftLeft: 0x2A,
    Backslash: 0x2B, KeyZ: 0x2C, KeyX: 0x2D, KeyC: 0x2E,
    KeyV: 0x2F, KeyB: 0x30, KeyN: 0x31, KeyM: 0x32,
    Comma: 0x33, Period: 0x34, Slash: 0x35, ShiftRight: 0x36,
    NumpadMultiply: 0x37, AltLeft: 0x38, Space: 0x39, CapsLock: 0x3A,
    F1: 0x3B, F2: 0x3C, F3: 0x3D, F4: 0x3E, F5: 0x3F,
    F6: 0x40, F7: 0x41, F8: 0x42, F9: 0x43, F10: 0x44,
    NumLock: 0x45, ScrollLock: 0x46, Home: 0x47, ArrowUp: 0x48,
    PageUp: 0x49, NumpadSubtract: 0x4A, ArrowLeft: 0x4B,
    ArrowRight: 0x4D, NumpadAdd: 0x4E, End: 0x4F, ArrowDown: 0x50,
    PageDown: 0x51, Insert: 0x52, Delete: 0x53,
    F11: 0x57, F12: 0x58,
    ControlRight: 0x1D, AltRight: 0x38,
    NumpadEnter: 0x1C, NumpadDivide: 0x35,
    Numpad0: 0x52, Numpad1: 0x4F, Numpad2: 0x50, Numpad3: 0x51,
    Numpad4: 0x4B, Numpad5: 0x4C, Numpad6: 0x4D, Numpad7: 0x47,
    Numpad8: 0x48, Numpad9: 0x49, NumpadDecimal: 0x53,
  };
  if (e.code && map[e.code] !== undefined) return map[e.code];
  return 0;
}

function keyToKeysym(e: KeyboardEvent): number {
  if (e.key.length === 1) return e.key.codePointAt(0) ?? 0;
  const map: Record<string, number> = {
    Enter: 0xff0d, Backspace: 0xff08, Tab: 0xff09, Escape: 0xff1b,
    ArrowUp: 0xff52, ArrowDown: 0xff54, ArrowLeft: 0xff51, ArrowRight: 0xff53,
    F1: 0xffbe, F2: 0xffbf, F3: 0xffc0, F4: 0xffc1,
    F5: 0xffc2, F6: 0xffc3, F7: 0xffc4, F8: 0xffc5,
    F9: 0xffc6, F10: 0xffc7, F11: 0xffc8, F12: 0xffc9,
    Delete: 0xffff, Home: 0xff50, End: 0xff57,
    PageUp: 0xff55, PageDown: 0xff56, Insert: 0xff63,
    Shift: 0xffe1, Control: 0xffe3, Alt: 0xffe9, Meta: 0xffe7,
    CapsLock: 0xffe5, ' ': 0x20,
  };
  return map[e.key] ?? 0;
}

/** 判断是否是 RDP 意义上的扩展键（需置 extended 位）。 */
function isExtendedKey(e: KeyboardEvent): boolean {
  return e.code === 'ControlRight'
    || e.code === 'AltRight'
    || e.code === 'NumpadEnter'
    || e.code === 'NumpadDivide'
    || e.code === 'ArrowUp' || e.code === 'ArrowDown'
    || e.code === 'ArrowLeft' || e.code === 'ArrowRight'
    || e.code === 'Home' || e.code === 'End'
    || e.code === 'PageUp' || e.code === 'PageDown'
    || e.code === 'Insert' || e.code === 'Delete';
}

/* ==========================================================================
   指针（光标）事件
   ========================================================================== */

interface RdpPointerEvent {
  type: 'default' | 'hidden' | 'position' | 'bitmap';
  sessionId: string;
  x?: number;
  y?: number;
  width?: number;
  height?: number;
  hotspotX?: number;
  hotspotY?: number;
  rgbaBase64?: string;
}

interface PointerState {
  bitmapUrl: string | null;
  hotspotX: number;
  hotspotY: number;
  hidden: boolean;
  useDefault: boolean;
}

const pointerStates = new Map<string, PointerState>();

function applyPointerEvent(s: RemoteSession, ev: RdpPointerEvent): void {
  let state = pointerStates.get(s.id);
  if (!state) {
    state = {
      bitmapUrl: null, hotspotX: 0, hotspotY: 0,
      hidden: false, useDefault: true,
    };
    pointerStates.set(s.id, state);
  }

  switch (ev.type) {
    case 'default':
      state.useDefault = true;
      state.bitmapUrl = null;
      state.hidden = false;
      break;
    case 'hidden':
      state.hidden = true;
      state.useDefault = false;
      state.bitmapUrl = null;
      break;
    case 'position':
      return;
    case 'bitmap':
      state.useDefault = false;
      state.hidden = false;
      state.hotspotX = ev.hotspotX ?? 0;
      state.hotspotY = ev.hotspotY ?? 0;
      state.bitmapUrl = ev.rgbaBase64 ? `data:image/png;base64,${ev.rgbaBase64}` : null;
      break;
  }
  redrawPointer(s, state);
}

function redrawPointer(s: RemoteSession, state: PointerState): void {
  if (state.hidden) {
    s.canvas.style.cursor = 'none';
    return;
  }
  if (state.useDefault || !state.bitmapUrl) {
    s.canvas.style.cursor = 'default';
    return;
  }
  s.canvas.style.cursor =
    `url(${state.bitmapUrl}) ${state.hotspotX} ${state.hotspotY}, default`;
}

/* ==========================================================================
   SPICE 光标（ARGB8888 → CSS cursor data URL）
   ========================================================================== */

interface SpiceCursorEvent {
  sessionId: string;
  kind: 'set' | 'from-cache' | 'move' | 'hide' | 'invalidate-one' | 'invalidate-all';
  x: number;
  y: number;
  hotX: number;
  hotY: number;
  width: number;
  height: number;
  pixelsBase64: string | null;
  unique: number;
  visible: boolean;
}

/** 每个 SPICE 会话的光标缓存（支持 from-cache 回放）。 */
const spiceCursorCache = new Map<string, Map<number, string>>();
/** 每个 SPICE 会话当前光标状态。 */
const spiceCursorState = new Map<string, { url: string | null; hotX: number; hotY: number; visible: boolean }>();

function applySpiceCursor(s: RemoteSession, ev: SpiceCursorEvent): void {
  let state = spiceCursorState.get(s.id);
  if (!state) {
    state = { url: null, hotX: 0, hotY: 0, visible: false };
    spiceCursorState.set(s.id, state);
  }

  switch (ev.kind) {
    case 'hide':
      state.visible = false;
      s.canvas.style.cursor = 'none';
      return;

    case 'invalidate-all':
      spiceCursorCache.get(s.id)?.clear();
      state.url = null;
      s.canvas.style.cursor = 'default';
      return;

    case 'invalidate-one':
      spiceCursorCache.get(s.id)?.delete(ev.unique);
      return;

    case 'from-cache': {
      const cached = spiceCursorCache.get(s.id)?.get(ev.unique);
      state.visible = ev.visible;
      if (!cached) {
        // 缓存里没有（可能已被 invalidate），退回默认光标
        state.url = null;
        s.canvas.style.cursor = 'default';
        return;
      }
      state.url = cached;
      // hotspot 由 set 时缓存，这里简化处理：直接复用
      s.canvas.style.cursor =
        `url(${cached}) ${state.hotX} ${state.hotY}, default`;
      return;
    }

    case 'move':
      // SPICE 的光标位置由 guest 绘制，客户端只负责样式；位置不变
      return;

    case 'set': {
      state.visible = ev.visible;
      state.hotX = ev.hotX;
      state.hotY = ev.hotY;
      if (!ev.pixelsBase64 || ev.width === 0 || ev.height === 0) {
        state.url = null;
        s.canvas.style.cursor = 'default';
        return;
      }

      // ARGB8888 → RGBA
      const bin = atob(ev.pixelsBase64);
      const argb = new Uint8Array(bin.length);
      for (let i = 0; i < bin.length; i++) argb[i] = bin.charCodeAt(i);

      const rgba = new Uint8ClampedArray(argb.length);
      for (let i = 0; i < argb.length; i += 4) {
        rgba[i]     = argb[i + 1]; // R
        rgba[i + 1] = argb[i + 2]; // G
        rgba[i + 2] = argb[i + 3]; // B
        rgba[i + 3] = argb[i];     // A
      }

      const off = document.createElement('canvas');
      off.width = ev.width;
      off.height = ev.height;
      const octx = off.getContext('2d')!;
      octx.putImageData(new ImageData(rgba, ev.width, ev.height), 0, 0);
      const url = off.toDataURL('image/png');

      // 缓存起来，供 from-cache 回放
      let cache = spiceCursorCache.get(s.id);
      if (!cache) { cache = new Map(); spiceCursorCache.set(s.id, cache); }
      cache.set(ev.unique, url);

      state.url = url;
      s.canvas.style.cursor =
        `url(${url}) ${ev.hotX} ${ev.hotY}, default`;
      return;
    }
  }
}

export function setupRemoteListeners(): void {

  listen<{
    sessionId: string;
    kind: 'set' | 'from-cache' | 'move' | 'hide' | 'invalidate-one' | 'invalidate-all';
    x: number; y: number;
    hotX: number; hotY: number;
    width: number; height: number;
    pixelsBase64: string | null;
    unique: number;
    visible: boolean;
  }>('spice-cursor', (event) => {
    const s = sessions.get(event.payload.sessionId);
    if (!s) return;
    applySpiceCursor(s, event.payload);
  }).catch(() => {});

  listen<{ sessionId: string; surfaceId: number; primary: boolean; width: number; height: number }>(
    'spice:active-surface',
    (event) => {
      const s = sessions.get(event.payload.sessionId);
      if (!s) return;
      s.spiceActiveSurfaceId = event.payload.surfaceId;
      s.canvas.width = event.payload.width;
      s.canvas.height = event.payload.height;
      s.width = event.payload.width;
      s.height = event.payload.height;
      s.ctx.fillStyle = '#000';
      s.ctx.fillRect(0, 0, s.width, s.height);
    },
  ).catch(() => {});

  listen<{ sessionId: string; mode: 'client' | 'server' }>(
    'spice-mouse-mode',
    (event) => {
      const s = sessions.get(event.payload.sessionId);
      if (s) s.spiceMouseMode = event.payload.mode;
    },
  ).catch(() => {});

  listen<{ sessionId: string }>('spice:closed', (event) => {
    const s = sessions.get(event.payload.sessionId);
    if (s) {
      s.canvas.remove();
      sessions.delete(event.payload.sessionId);
      drawingSessions.delete(event.payload.sessionId);
      pointerStates.delete(event.payload.sessionId);
    }
  }).catch(() => {});

  listen<{ sessionId: string; width: number; height: number }>(
    'vnc:resolution',
    (event) => {
      const { sessionId, width, height } = event.payload;
      const s = sessions.get(sessionId);
      if (s) {
        s.canvas.width = width;
        s.canvas.height = height;
        s.width = width;
        s.height = height;
        s.ctx.fillStyle = '#000';
        s.ctx.fillRect(0, 0, width, height);
      } else {
        pendingResolutions.set(sessionId, { width, height });
      }
    },
  ).catch((e) => console.error('[RD] vnc:resolution 监听注册失败:', e));

  listen<{ sessionId: string }>('vnc:closed', (event) => {
    const s = sessions.get(event.payload.sessionId);
    if (s) {
      s.canvas.remove();
      sessions.delete(event.payload.sessionId);
      drawingSessions.delete(event.payload.sessionId);
      pointerStates.delete(event.payload.sessionId);
    }
  }).catch(() => {});

    // VNC 服务器 key 验证请求
  listen<{
    requestId: string;
    sessionId: string;
    host: string;
    port: number;
    fingerprint: string;
    keyBits: number;
    knownHostStatus: 'match' | 'changed' | 'unknown';
  }>('vnc-server-key-verify', (event) => {
    void handleVncServerKeyVerify(event.payload);
  }).catch(() => {});

  listen<RdpPointerEvent>('rdp:pointer', (event) => {
    const payload = event.payload;
    const s = sessions.get(payload.sessionId);
    if (!s) return;
    applyPointerEvent(s, payload);
  }).catch(() => {});

  listen<RdpCertificateVerifyEvent>('rdp-certificate-verify', (event) => {
    void handleCertificateVerify(event.payload);
  }).catch(() => {});

  listen<{ sessionId: string }>('rdp:closed', (event) => {
    const s = sessions.get(event.payload.sessionId);
    if (s) {
      s.canvas.remove();
      sessions.delete(event.payload.sessionId);
      drawingSessions.delete(event.payload.sessionId);
      rdpInputQueues.delete(event.payload.sessionId);
      pointerStates.delete(event.payload.sessionId);
    }
  }).catch(() => {});
}

export async function closeRemote(sessionId: string, type: 'vnc' | 'rdp' | 'spice'): Promise<void> {
  if (type === 'vnc') {
    await invoke('vnc_close', { sessionId }).catch(() => {});
    vncInputQueues.delete(sessionId);
  } else if (type === 'rdp') {
    try {
      await invoke('rdp_close', { sessionId });
    } finally {
      rdpInputQueues.delete(sessionId);
      pointerStates.delete(sessionId);
    }
  } else {
    await invoke('spice_close', { sessionId }).catch(() => {});
    spiceInputQueues.delete(sessionId);
    pressedSpiceKeys.delete(sessionId);
    spiceCursorCache.delete(sessionId);
    spiceCursorState.delete(sessionId);
  }
  const s = sessions.get(sessionId);
  if (s) {
    s.canvas.remove();
    sessions.delete(sessionId);
    drawingSessions.delete(sessionId);
  }
}

/**
 * native 模式下窗口缩放时同步 RDP 分辨率。
 * 由 main.ts 在窗口 resize 时调用。
 */
export async function resizeRemote(
  sessionId: string,
  width: number,
  height: number,
): Promise<void> {
  await invoke('rdp_resize', { sessionId, width, height }).catch(() => {});
}

interface RdpCertificateVerifyEvent {
  requestId: string;
  sessionId: string;
  host: string;
  port: number;
  fingerprint: string;
  subject: string | null;
  issuer: string | null;
  validFrom: string | null;
  validTo: string | null;
  knownHostStatus: 'match' | 'changed' | 'unknown';
}

/**
 * 一个最小的自绘 confirm 模态框。
 *
 * 不能用 window.confirm：Tauri 2 的 WebView 会拦截原生 confirm，
 * 走 dialog 插件（plugin:dialog|confirm），而该命令又受 ACL 限制，
 * 未在 capabilities 里声明就报 "not allowed"，导致确认框根本弹不出来。
 *
 * 用自绘的 DOM 模态框绕开 Tauri 的 IPC，纯前端实现。
 */
function simpleConfirm(title: string, message: string): Promise<boolean> {
  return new Promise((resolve) => {
    const overlay = document.createElement('div');
    overlay.style.cssText = `
      position: fixed; inset: 0; z-index: 99999;
      display: flex; align-items: center; justify-content: center;
      background: rgba(0, 0, 0, 0.4);
    `;

    const box = document.createElement('div');
    box.style.cssText = `
      max-width: 560px; min-width: 320px; max-height: 80vh;
      overflow-y: auto; padding: 20px 24px;
      background: var(--bg-card, #fff); color: var(--text, #1a1a1a);
      border: 1px solid var(--border, #ccc);
      border-radius: 4px; font-size: 13px;
      box-shadow: 0 4px 16px rgba(0, 0, 0, 0.28);
    `;

    const h = document.createElement('div');
    h.style.cssText = 'font-weight: 600; font-size: 15px; margin-bottom: 12px;';
    h.textContent = title;

    const p = document.createElement('div');
    p.style.cssText = 'white-space: pre-wrap; word-break: break-all; line-height: 1.5; margin-bottom: 16px;';
    p.textContent = message;

    const btnRow = document.createElement('div');
    btnRow.style.cssText = 'display: flex; gap: 8px; justify-content: flex-end;';

    const cancelBtn = document.createElement('button');
    cancelBtn.textContent = '取消';
    cancelBtn.style.cssText = `
      padding: 5px 16px; border: 1px solid var(--border, #ccc);
      background: var(--bg-panel, #f5f5f5); color: var(--text, #000);
      border-radius: 2px; cursor: pointer; font-size: 12px;
    `;

    const okBtn = document.createElement('button');
    okBtn.textContent = '接受';
    okBtn.style.cssText = `
      padding: 5px 16px; border: 1px solid var(--accent, #3a7bd5);
      background: var(--accent, #3a7bd5); color: #fff;
      border-radius: 2px; cursor: pointer; font-size: 12px;
    `;

    const cleanup = (value: boolean) => {
      overlay.remove();
      resolve(value);
    };
    cancelBtn.onclick = () => cleanup(false);
    okBtn.onclick = () => cleanup(true);
    overlay.addEventListener('keydown', (e) => {
      if (e.key === 'Escape') cleanup(false);
      else if (e.key === 'Enter') cleanup(true);
    });

    btnRow.append(cancelBtn, okBtn);
    box.append(h, p, btnRow);
    overlay.appendChild(box);
    document.body.appendChild(overlay);
    okBtn.focus();
  });
}

async function handleCertificateVerify(ev: RdpCertificateVerifyEvent): Promise<void> {
  const lines = [
    `服务器: ${ev.host}:${ev.port}`,
    `指纹: ${ev.fingerprint}`,
  ];
  if (ev.subject) lines.push(`主体: ${ev.subject}`);
  if (ev.issuer) lines.push(`颁发者: ${ev.issuer}`);
  if (ev.validFrom) lines.push(`生效: ${ev.validFrom}`);
  if (ev.validTo) lines.push(`到期: ${ev.validTo}`);

  const title = ev.knownHostStatus === 'changed'
    ? '⚠️ 证书指纹已变更！这可能意味着中间人攻击。'
    : ev.knownHostStatus === 'unknown'
      ? '首次连接，证书未记录。'
      : '证书验证';

  const accepted = await simpleConfirm(
    title,
    `${lines.join('\n')}\n\n是否接受此证书？`,
  );
  let remember = false;
  if (accepted) {
    remember = await simpleConfirm('记住证书', '是否永久记住此证书？（推荐）');
  }

  await invoke('rdp_respond_certificate', {
    requestId: ev.requestId,
    accepted,
    remember,
  }).catch((e) => console.error('[RD] 证书响应失败:', e));
}

interface VncServerKeyVerifyEvent {
  requestId: string;
  sessionId: string;
  host: string;
  port: number;
  fingerprint: string;
  keyBits: number;
  knownHostStatus: 'match' | 'changed' | 'unknown';
}

async function handleVncServerKeyVerify(ev: VncServerKeyVerifyEvent): Promise<void> {
  const title = ev.knownHostStatus === 'changed'
    ? '⚠️ 服务器密钥已变更！这可能意味着中间人攻击。'
    : ev.knownHostStatus === 'unknown'
      ? '首次连接，服务器密钥未记录。'
      : '服务器密钥验证';

  const lines = [
    `服务器: ${ev.host}:${ev.port}`,
    `密钥位数: ${ev.keyBits}`,
    `指纹: ${ev.fingerprint}`,
  ];

  const accepted = await simpleConfirm(
    title,
    `${lines.join('\n')}\n\n是否接受此服务器密钥？`,
  );

  await invoke('vnc_respond_server_key', {
    requestId: ev.requestId,
    accepted,
  }).catch((e) => console.error('[RD] VNC server key 响应失败:', e));
}