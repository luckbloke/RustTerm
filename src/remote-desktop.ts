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
}

const sessions = new Map<string, RemoteSession>();
const pendingResolutions = new Map<string, { width: number; height: number }>();
// 防止同一 session 的帧堆积：上一帧还没画完就丢弃新帧
const drawingSessions = new Set<string>();

const MOUSE_INTERVAL_MS = 16;

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
  canvas.width = 1280;
  canvas.height = 720;
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
  if (type === 'vnc') {
    const frameChannel = new Channel<ArrayBuffer>();
    frameChannel.onmessage = (raw) => {
      handleBinaryFrame(sessionId, raw);
    };
    sessionId = await invoke<string>('vnc_connect', {
      host, port, password,
      frameChannel,
    });
  } else if (type === 'rdp') {
    const frameChannel = new Channel<ArrayBuffer>();
    frameChannel.onmessage = (raw) => {
      handleBinaryFrame(sessionId, raw);
    };
    sessionId = await invoke<string>('rdp_connect', {
      host, port, username, password,
      width: canvas.width, height: canvas.height,
      frameChannel,
    });
  } else if (type === 'spice') {
    sessionId = await invoke<string>('spice_connect', { host, port, password });
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
      invoke('vnc_send_key', { sessionId, keysym, down: true }).catch(() => {});
    } else if (type === 'rdp') {
      const sc = keyToScancode(e);
      if (sc === 0) return;
      invoke('rdp_send_key', { sessionId, code: sc, down: true }).catch(() => {});
    } else if (type === 'spice') {
      const sc = keyToScancode(e);
      if (sc === 0) return;
      invoke('spice_send_key', { sessionId, scancode: sc, down: true }).catch(() => {});
    }
  });

  canvas.addEventListener('keyup', (e) => {
    if (type === 'vnc') {
      const keysym = keyToKeysym(e);
      if (keysym === 0) return;
      invoke('vnc_send_key', { sessionId, keysym, down: false }).catch(() => {});
    } else if (type === 'rdp') {
      const sc = keyToScancode(e);
      if (sc === 0) return;
      invoke('rdp_send_key', { sessionId, code: sc, down: false }).catch(() => {});
    } else if (type === 'spice') {
      const sc = keyToScancode(e);
      if (sc === 0) return;
      invoke('spice_send_key', { sessionId, scancode: sc, down: false }).catch(() => {});
    }
  });

  canvas.addEventListener('contextmenu', (e) => e.preventDefault());

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
}

function sendPointerVnc(sessionId: string, canvas: HTMLCanvasElement, e: MouseEvent): void {
  const rect = canvas.getBoundingClientRect();
  const x = Math.round(((e.clientX - rect.left) / rect.width) * canvas.width);
  const y = Math.round(((e.clientY - rect.top) / rect.height) * canvas.height);
  let rfbButtons = 0;
  if (e.buttons & 1) rfbButtons |= 1;
  if (e.buttons & 2) rfbButtons |= 4;
  if (e.buttons & 4) rfbButtons |= 2;
  invoke('vnc_send_pointer', { sessionId, x, y, buttons: rfbButtons }).catch(() => {});
}

function sendPointerRdp(sessionId: string, canvas: HTMLCanvasElement, e: MouseEvent, down: boolean): void {
  const rect = canvas.getBoundingClientRect();
  const x = Math.round(((e.clientX - rect.left) / rect.width) * canvas.width);
  const y = Math.round(((e.clientY - rect.top) / rect.height) * canvas.height);
  const button = e.button === 0 ? 'left' : e.button === 2 ? 'right' : 'middle';
  invoke('rdp_send_pointer', { sessionId, x, y, button, down }).catch(() => {});
}

function sendPointerRdpMove(sessionId: string, canvas: HTMLCanvasElement, e: MouseEvent): void {
  const rect = canvas.getBoundingClientRect();
  const x = Math.round(((e.clientX - rect.left) / rect.width) * canvas.width);
  const y = Math.round(((e.clientY - rect.top) / rect.height) * canvas.height);
  invoke('rdp_send_pointer', { sessionId, x, y, button: 'none', down: false }).catch(() => {});
}

function sendPointerSpice(sessionId: string, canvas: HTMLCanvasElement, e: MouseEvent, down: boolean): void {
  const rect = canvas.getBoundingClientRect();
  const x = Math.round(((e.clientX - rect.left) / rect.width) * canvas.width);
  const y = Math.round(((e.clientY - rect.top) / rect.height) * canvas.height);
  const button = e.button === 0 ? 1 : e.button === 2 ? 3 : 2;
  invoke('spice_send_pointer', { sessionId, x, y, button, down }).catch(() => {});
}

function sendPointerSpiceMove(sessionId: string, canvas: HTMLCanvasElement, e: MouseEvent): void {
  const rect = canvas.getBoundingClientRect();
  const x = Math.round(((e.clientX - rect.left) / rect.width) * canvas.width);
  const y = Math.round(((e.clientY - rect.top) / rect.height) * canvas.height);
  invoke('spice_send_pointer', { sessionId, x, y, button: null, down: null }).catch(() => {});
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

function handleSpiceFrame(
  sessionId: string,
  width: number,
  height: number,
  jpeg: number[],
): void {
  const s = sessions.get(sessionId);
  if (!s) return;
  if (s.canvas.width !== width || s.canvas.height !== height) {
    s.canvas.width = width;
    s.canvas.height = height;
    s.width = width;
    s.height = height;
  }
  const blob = new Blob([new Uint8Array(jpeg)], { type: 'image/jpeg' });
  void createImageBitmap(blob).then((bitmap) => {
    s.ctx.drawImage(bitmap, 0, 0);
    bitmap.close();
  });
}

export function setupRemoteListeners(): void {
  listen<{ sessionId: string; width: number; height: number; data: number[] }>(
    'spice:frame',
    (event) => {
      const { sessionId, width, height, data } = event.payload;
      handleSpiceFrame(sessionId, width, height, data);
    },
  ).catch((e) => console.error('[RD] spice:frame 监听注册失败:', e));

  listen<{ sessionId: string }>('spice:closed', (event) => {
    const s = sessions.get(event.payload.sessionId);
    if (s) {
      s.canvas.remove();
      sessions.delete(event.payload.sessionId);
      drawingSessions.delete(event.payload.sessionId);
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
    }
  }).catch(() => {});

  listen<{ sessionId: string }>('rdp:closed', (event) => {
    const s = sessions.get(event.payload.sessionId);
    if (s) {
      s.canvas.remove();
      sessions.delete(event.payload.sessionId);
      drawingSessions.delete(event.payload.sessionId);
    }
  }).catch(() => {});
}

export async function closeRemote(sessionId: string, type: 'vnc' | 'rdp' | 'spice'): Promise<void> {
  if (type === 'vnc') await invoke('vnc_close', { sessionId }).catch(() => {});
  else if (type === 'rdp') await invoke('rdp_close', { sessionId }).catch(() => {});
  else await invoke('spice_close', { sessionId }).catch(() => {});
  const s = sessions.get(sessionId);
  if (s) {
    s.canvas.remove();
    sessions.delete(sessionId);
    drawingSessions.delete(sessionId);
  }
}