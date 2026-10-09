import { getVersion } from '@tauri-apps/api/app';
import { Terminal, type IDisposable, type ITheme } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { WebglAddon } from '@xterm/addon-webgl';
import { SearchAddon } from '@xterm/addon-search';
import { WebLinksAddon } from '@xterm/addon-web-links';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { open as openDialog, save as saveDialog } from '@tauri-apps/plugin-dialog';
import { getCurrentWindow } from '@tauri-apps/api/window';
import {
  applyI18n, errorText, loadLang, makeT, saveLang, type Lang, type T,
} from './i18n';
import '@xterm/xterm/css/xterm.css';
import { connectRemote, closeRemote, setupRemoteListeners } from './remote-desktop';

// 注册 VNC/RDP 事件监听（全局一次）
setupRemoteListeners();

// RDP 状态事件：更新状态栏
void listen<{
  sessionId: string;
  state: string;
  message: string | null;
  errorKind: string | null;
}>('rdp:state', (event) => {
  const { state, message, errorKind } = event.payload;
  setStatus(rdpStateLabel(state, message, errorKind));
});

function rdpStateLabel(
  state: string,
  message: string | null,
  errorKind: string | null,
): string {
  switch (state) {
    case 'connecting':
      return t('rdpStateConnecting');
    case 'authenticating':
      return t('rdpStateAuthenticating');
    case 'active':
      return message ?? t('rdpStateActive');
    case 'reconnecting':
      return message ?? t('rdpStateReconnecting');
    case 'disconnected':
      return message ?? t('rdpStateDisconnected');
    case 'failed':
      return errorKind
        ? t(`rdpError_${errorKind}` as any) || message || t('rdpStateFailed')
        : message ?? t('rdpStateFailed');
    default:
      return state;
  }
}

/* ==========================================================================
   1. 主题与语言
   ========================================================================== */

const TERM_THEMES: Record<'light' | 'dark', ITheme> = {
  light: {
    background: '#ffffff',
    foreground: '#333333',
    cursor: '#333333',
    selectionBackground: '#b5d6fd',
    black: '#000000', red: '#cd3131', green: '#00bc00', yellow: '#949800',
    blue: '#0451a5', magenta: '#bc05bc', cyan: '#0598bc', white: '#555555',
    brightBlack: '#666666', brightRed: '#cd3131', brightGreen: '#14ce14',
    brightYellow: '#b5ba00', brightBlue: '#0451a5', brightMagenta: '#bc05bc',
    brightCyan: '#0598bc', brightWhite: '#a5a5a5',
  },
  dark: {
    background: '#1e1e1e',
    foreground: '#dddddd',
    cursor: '#dddddd',
    selectionBackground: '#264f78',
    black: '#000000', red: '#cd3131', green: '#0dbc79', yellow: '#e5e510',
    blue: '#2472c8', magenta: '#bc3fbc', cyan: '#11a8cd', white: '#e5e5e5',
    brightBlack: '#666666', brightRed: '#f14c4c', brightGreen: '#23d18b',
    brightYellow: '#f5f543', brightBlue: '#3b8eea', brightMagenta: '#d670d6',
    brightCyan: '#29b8db', brightWhite: '#e5e5e5',
  },
};

type Theme = 'light' | 'dark';
const THEME_KEY = 'rustterm.theme';
const FONT_KEY = 'rustterm.fontSize';
const MACRO_KEY = 'rustterm.macros';
const HOSTKEY_KEY = 'rustterm.hostKeyPolicy';
const SHOW_HIDDEN_KEY = 'rustterm.sftpShowHidden';
const FONT_STACK = 'Consolas, "Cascadia Mono", "Courier New", monospace';

/** 主机密钥校验策略，与后端 hostkey::HostKeyPolicy 的取值一一对应。 */
type HostKeyPolicy = 'strict' | 'accept-new' | 'insecure';

let hostKeyPolicy: HostKeyPolicy =
  (localStorage.getItem(HOSTKEY_KEY) as HostKeyPolicy) || 'accept-new';

let lang: Lang = loadLang();
let t: T = makeT(lang);
let theme: Theme = (localStorage.getItem(THEME_KEY) as Theme) === 'dark' ? 'dark' : 'light';

/* ==========================================================================
   2. 全局状态
   ========================================================================== */

interface SplitPane {
  term: Terminal;
  fit: FitAddon;
  sessionId: string;
  pane: HTMLElement;
  disposables: IDisposable[];
}

interface Tab {
  id: number;
  title: string;
  sessionId: string;
  isSsh: boolean;
  sftpPath: string;
  /** 目录列表请求序号，用于丢弃过期响应 */
  sftpRequest: number;
  syncGroup: number;
  color: string;
  /** 该标签页独立的终端实例 */
  term: Terminal;
  fit: FitAddon;
  /** 承载该标签页全部窗格的包装层（分屏时为两个窗格的父元素） */
  wrapper: HTMLElement;
  /** 主窗格：主终端打开的元素 */
  pane: HTMLElement;
  split: SplitPane | null;
  search: SearchAddon | null;
  disposables: IDisposable[];
  /** 已关闭标记：dispose 之后禁止再向后端发请求 */
  closed: boolean;
  /** 后端会话已结束（shell 退出/连接断开），但标签还留着供查看输出 */
  disconnected: boolean;
  /** 远程桌面会话（VNC/RDP/SPICE）：term 是空壳，内容由 canvas 渲染 */
  remote?: { sessionId: string; type: 'vnc' | 'rdp' | 'spice' } | null;
}

interface TransferJob {
  id: string;
  sessionId: string;
  kind: 'upload' | 'download';
  local: string;
  remote: string;
  label: string;
}

interface SavedSession {
  name: string;
  host: string;
  port: number;
  user: string;
  group: string;
  color: string;
  /** 密码是否已存入系统凭据库（密码本身不在 sessions.json 里） */
  save_password: boolean;
  /** 'ssh' | 'vnc' | 'rdp' | 'spice'，旧存档缺省按 'ssh' 处理 */
  protocol?: string;
}

interface RemoteEntry {
  name: string;
  path: string;
  is_dir: boolean;
  size: number;
  /** 隐藏项（以 . 开头或远端标记为隐藏）；是否显示由界面开关决定 */
  hidden: boolean;
}

/**
 * 远程桌面标签（VNC/RDP）的原始连接信息。
 * tab.title 只反映 host，port 和 protocol 会丢，保存会话时需要这份记录。
 */
 const remoteTabInfo = new Map<number, { host: string; port: number; protocol: 'vnc' | 'rdp' | 'spice' }>();

let tabs: Tab[] = [];
let activeTab = 0;
let nextId = 1;
let fontSize = parseInt(localStorage.getItem(FONT_KEY) || '', 10) || 14;
let recording: number[] | null = null;
let recordedMacros: { name: string; data: number[] }[] = loadMacros();
let currentTransferId: string | null = null;
/** 断点续传不走队列，单独记录它的 id，避免与队列项互相顶掉 */
let resumeTransferId: string | null = null;
let isFullscreen = false;
let cachedSessions: SavedSession[] = [];
let welcomeFilter = '';

/** SFTP 是否显示隐藏文件（以 . 开头或带隐藏属性）。缺省不显示。 */
let showHiddenFiles = localStorage.getItem(SHOW_HIDDEN_KEY) === '1';

const transferQueue: TransferJob[] = [];
let transferRunning = false;

/* --- 常用节点 --- */
const $ = <E extends HTMLElement = HTMLElement>(id: string): E => document.getElementById(id) as E;
const tabsEl = $('tabs');
const statusEl = $('status');
const statusPosEl = $('status-pos');
const panesEl = $('terminal-panes');
const welcomeEl = $('welcome');
const sftpPanel = $('sftp-panel');
const sftpListEl = $<HTMLUListElement>('sftp-list');
const sftpStatusEl = $('sftp-status');
const sftpProgress = $('sftp-progress');
const sftpProgressLabel = $('sftp-progress-label');
const sftpProgressFill = $('sftp-progress-fill');
const findBar = $('find-bar');
const findInput = $<HTMLInputElement>('find-input');

/* ==========================================================================
   3. 基础工具
   ========================================================================== */

function setStatus(msg: string): void {
  statusEl.textContent = msg;
}

/** 光标位置单独一格，避免每次移动都覆盖状态正文。 */
let posPending = false;
function setPosition(row: number, col: number): void {
  if (posPending) return;
  posPending = true;
  requestAnimationFrame(() => {
    posPending = false;
    statusPosEl.textContent = t('statusPos', { row, col });
  });
}

function currentTab(): Tab | undefined {
  return tabs[activeTab];
}

function writeBytes(sessionId: string, bytes: number[]): void {
  void invoke('pty_write', { sessionId, data: bytes }).catch(() => {});
}

function encode(text: string): number[] {
  return Array.from(new TextEncoder().encode(text));
}

function humanSize(bytes: number): string {
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let size = bytes;
  let unit = 0;
  while (size >= 1024 && unit < units.length - 1) { size /= 1024; unit++; }
  return unit === 0 ? `${bytes} B` : `${size.toFixed(1)} ${units[unit]}`;
}

function loadMacros(): { name: string; data: number[] }[] {
  try {
    const raw = localStorage.getItem(MACRO_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw);
    return Array.isArray(parsed) ? parsed : [];
  } catch { return []; }
}

function saveMacros(): void {
  try { localStorage.setItem(MACRO_KEY, JSON.stringify(recordedMacros)); } catch { /* 容量不足 */ }
}

/* ==========================================================================
   4. 对话框
   ========================================================================== */

/** 记住当前打开的输入框，语言切换时能就地重绘。 */
let openPrompt: { title: string; message: string; params?: Record<string, unknown> } | null = null;

/**
 * 是否已有模态框打开。
 *
 * 三个对话框共用同一套 DOM 与按钮，如果允许第二个弹窗在第一个还没回答时打开，
 * 它会覆盖掉前一个的 onclick 回调，导致先前那个 await 永远不 resolve
 * （连接、保存等流程会静默卡死）。原生菜单栏不受 HTML 模态框遮挡，
 * 所以这种重入是真实可达的。忙时直接按"用户取消"返回。
 */
let dialogBusy = false;

function showPrompt(
  titleKey: string,
  messageKey: string,
  defaultValue = '',
  password = false,
  params?: Record<string, unknown>,
): Promise<string | null> {
  if (dialogBusy) { setStatus(t('statusDialogBusy')); return Promise.resolve(null); }
  dialogBusy = true;
  return new Promise((resolve) => {
    const modal = $('prompt-modal');
    openPrompt = { title: titleKey, message: messageKey, params };
    $('prompt-title').textContent = t(titleKey);
    $('prompt-message').textContent = t(messageKey, params);
    const input = $<HTMLInputElement>('prompt-input');
    input.type = password ? 'password' : 'text';
    input.value = defaultValue;
    modal.classList.remove('hidden');

    const ok = $<HTMLButtonElement>('prompt-ok');
    const cancel = $<HTMLButtonElement>('prompt-cancel');
    const cleanup = () => {
      modal.classList.add('hidden');
      openPrompt = null;
      dialogBusy = false;
      ok.onclick = null;
      cancel.onclick = null;
      input.onkeydown = null;
    };
    ok.onclick = () => { const value = input.value; cleanup(); resolve(value); };
    cancel.onclick = () => { cleanup(); resolve(null); };
    input.onkeydown = (e) => {
      if (e.key === 'Enter') { const value = input.value; cleanup(); resolve(value); }
      else if (e.key === 'Escape') { cleanup(); resolve(null); }
    };
    setTimeout(() => { input.focus(); input.select(); }, 30);
  });
}

function showConfirm(
  titleKey: string,
  messageKey: string,
  params?: Record<string, unknown>,
  buttons?: { ok: string; cancel: string },
): Promise<boolean> {
  if (dialogBusy) { setStatus(t('statusDialogBusy')); return Promise.resolve(false); }
  dialogBusy = true;
  return new Promise((resolve) => {
    const modal = $('confirm-modal');
    $('confirm-title').textContent = t(titleKey);
    $('confirm-message').textContent = t(messageKey, params);
    const ok = $<HTMLButtonElement>('confirm-ok');
    const cancel = $<HTMLButtonElement>('confirm-cancel');
    // 每次重新设置，避免上一次的自定义文本残留
    ok.textContent = buttons?.ok ?? t('commonConfirm');
    cancel.textContent = buttons?.cancel ?? t('commonCancel');
    modal.classList.remove('hidden');
    const cleanup = () => {
      modal.classList.add('hidden');
      dialogBusy = false;
      ok.onclick = null;
      cancel.onclick = null;
    };
    ok.onclick = () => { cleanup(); resolve(true); };
    cancel.onclick = () => { cleanup(); resolve(false); };
  });
}

function showAlert(titleKey: string, message: string): Promise<void> {
  if (dialogBusy) { setStatus(t('statusDialogBusy')); return Promise.resolve(); }
  dialogBusy = true;
  return new Promise((resolve) => {
    const modal = $('alert-modal');
    $('alert-title').textContent = t(titleKey);
    $('alert-message').textContent = message;
    modal.classList.remove('hidden');
    const ok = $<HTMLButtonElement>('alert-ok');
    ok.onclick = () => {
      modal.classList.add('hidden');
      dialogBusy = false;
      ok.onclick = null;
      resolve();
    };
  });
}

/** 后端错误码 → 当前语言的可读文案。 */
function errText(err: unknown): string {
  return errorText(t, err);
}

/* ==========================================================================
   5. 标签页生命周期
   ========================================================================== */

function renderTabs(): void {
  tabsEl.textContent = '';
  tabs.forEach((tab, index) => {
    const el = document.createElement('div');
    el.className = 'tab'
      + (index === activeTab ? ' active' : '')
      + (tab.disconnected ? ' disconnected' : '');
    el.draggable = true;
    if (tab.color) el.style.borderLeft = `4px solid ${tab.color}`;

    const title = document.createElement('span');
    title.className = 'tab-title';
    title.textContent = tab.title;
    el.appendChild(title);

    if (tab.remote) {
      const badge = document.createElement('span');
      badge.className = 'tab-badge';
      badge.textContent = tab.remote.type.toUpperCase();
      el.appendChild(badge);
    }

    if (tab.syncGroup > 0) {
      const badge = document.createElement('span');
      badge.className = 'tab-badge';
      badge.textContent = `G${tab.syncGroup}`;
      el.appendChild(badge);
    }
    if (tab.split) {
      const badge = document.createElement('span');
      badge.className = 'tab-badge';
      badge.textContent = '▥';
      el.appendChild(badge);
    }

    const close = document.createElement('span');
    close.className = 'close';
    close.textContent = '×';
    close.onclick = (e) => { e.stopPropagation(); closeTab(tab); };
    el.appendChild(close);

    el.onclick = () => activateTab(index);
    el.oncontextmenu = (e) => { e.preventDefault(); void showTabMenu(tab); };
    el.ondragstart = (e) => { e.dataTransfer?.setData('text/plain', String(index)); el.classList.add('dragging'); };
    el.ondragend = () => el.classList.remove('dragging');
    el.ondragover = (e) => e.preventDefault();
    el.ondrop = (e) => {
      e.preventDefault();
      const from = parseInt(e.dataTransfer?.getData('text/plain') || '-1', 10);
      if (from < 0 || from === index || from >= tabs.length) return;
      const wasActive = tabs[activeTab];
      const [moved] = tabs.splice(from, 1);
      tabs.splice(index, 0, moved);
      // 让活动标签仍然指向原来那个标签，而不是某个固定下标
      activeTab = Math.max(0, tabs.indexOf(wasActive));
      renderTabs();
      setActivePanes();
      saveOpenTabs();
    };

    tabsEl.appendChild(el);
  });
}

async function showTabMenu(tab: Tab): Promise<void> {
  const action = await showPrompt('tabMenuTitle', 'tabMenuBody', '', false, { title: tab.title });
  if (!action) return;
  if (action === '1') {
    const name = await showPrompt('tabRename', 'tabRenamePrompt', tab.title);
    if (name) { tab.title = name; renderTabs(); saveOpenTabs(); }
  } else if (action === '2') {
    closeTab(tab);
  } else if (action === '3') {
    // 先记住目标标签的原始下标：关闭其他标签后列表会收缩，indexOf 不再等于它。
    const keepIndex = tabs.indexOf(tab);
    for (const other of [...tabs]) if (other !== tab) closeTab(other, true);
    activeTab = tabs.indexOf(tab);
    if (activeTab < 0) activeTab = Math.max(0, Math.min(keepIndex, tabs.length - 1));
    renderTabs();
    setActivePanes();
    refreshSftpVisibility();
    if (tabs.length === 0) showWelcome();
    else fitActive();
    saveOpenTabs();
  } else if (action === '4') {
    const color = await showPrompt('tabColor', 'tabColorPrompt', tab.color || '');
    if (color !== null) { tab.color = color.trim(); renderTabs(); saveOpenTabs(); }
  }
}

function createPane(tab: Tab, paneClass: string): HTMLElement {
  const pane = document.createElement('div');
  pane.className = `term-pane ${paneClass}`;
  tab.wrapper.appendChild(pane);
  return pane;
}

function attachTerminal(term: Terminal, pane: HTMLElement): void {
  term.open(pane);
  try {
    const webgl = new WebglAddon();
    // 多标签时 WebGL 上下文会被回收，上下文一丢整块终端会变空白且不会自动恢复；
    // 主动 dispose 该 addon，xterm 会回退到 DOM 渲染器继续可用。
    webgl.onContextLoss(() => {
      console.warn('WebGL context lost, falling back to DOM renderer');
      webgl.dispose();
    });
    term.loadAddon(webgl);
  } catch (e) {
    // WebGL 不可用时 xterm 自动退回 DOM 渲染器
    console.warn('WebGL renderer unavailable, falling back', e);
  }
}

/**
 * 给终端绑自定义按键处理。
 *
 * 关键冲突：终端里的 Ctrl+C 本职是发送 0x03（SIGINT），不是"复制"。
 * 用户选中一段文字后按 Ctrl+C，xterm 默认会把它当控制字符发给远端，
 * 命令被中断，而选中的文字压根没进剪贴板——这是所有终端模拟器的老问题。
 *
 * 处理方式对齐 Windows Terminal / MobaXterm 的惯例：
 *   - 有选区时 Ctrl+C → 复制，不发给远端
 *   - 无选区时 Ctrl+C → 交回 xterm，正常发送 SIGINT
 *   - Ctrl+V → 从剪贴板粘贴
 * 其余按键一律不拦截，避免影响 readline / vim 之类的快捷键。
 */
function attachKeyHandler(term: Terminal): void {
  term.attachCustomKeyEventHandler((event) => {
    // 一次按键会触发 keydown/keypress/keyup 三次回调，只处理 keydown。
    if (event.type !== 'keydown') return true;
    // 带 Shift / Alt 的组合键不拦，留给系统和终端自己处理。
    if (!event.ctrlKey || event.shiftKey || event.altKey) return true;

    const key = event.key.toLowerCase();

    if (key === 'c') {
      if (term.hasSelection()) {
        void navigator.clipboard.writeText(term.getSelection()).then(
          () => setStatus(t('statusCopyMenuCopied')),
          (err) => setStatus(t('statusCopyFailed', { err: errText(err) })),
        );
        // 返回 false 表示事件已消费，xterm 不再把 0x03 发给远端。
        return false;
      }
      // 没有选区，交给 xterm 发送 SIGINT
      return true;
    }

    if (key === 'v') {
      void navigator.clipboard.readText().then(
        (text) => {
          const tab = currentTab();
          if (tab && text && !tab.disconnected) sendInput(tab, text);
        },
        (err) => setStatus(t('statusPasteFailed', { err: errText(err) })),
      );
      return false;
    }

    return true;
  });
}

/**
 * 创建一个"远程桌面"标签。
 *
 * 和普通终端标签的区别：
 * - 不创建 xterm / FitAddon，因为 VNC/RDP 用 canvas 渲染
 * - wrapper 里放一个 canvas，由 connectRemote 填充
 * - term 用一个空壳 xterm 占位，避免改动所有用到 tab.term 的代码路径
 *   （实际上不会 open、不会 write、不会 dispose 时出问题）
 */
function createRemoteTab(sessionId: string, title: string, type: 'vnc' | 'rdp' | 'spice'): Tab {
  const wrapper = document.createElement('div');
  wrapper.className = 'term-pane';
  panesEl.appendChild(wrapper);

  // 空壳终端：不 open，不 loadAddon，不接事件。仅用于类型占位。
  const term = new Terminal({ cursorBlink: false });
  const fit = new FitAddon();
  // 不调用 term.open()，所以这个 term 不会渲染任何东西。
  // 所有对 tab.term 的调用都需要在下游做守卫（见下面的 fitTab / sendInput 等）。

  const tab: Tab = {
    id: nextId++, title, sessionId, isSsh: false,
    sftpPath: '.', sftpRequest: 0, syncGroup: 0, color: '',
    term, fit, wrapper, pane: wrapper, split: null, search: null,
    disposables: [], closed: false, disconnected: false,
    remote: { sessionId, type },
  };

  // 只挂 canvas 需要的输入处理。xterm 的事件一律不接。
  tabs.push(tab);
  activeTab = tabs.length - 1;
  renderTabs();
  hideWelcome();
  setActivePanes();
  saveOpenTabs();
  return tab;
}

function createTab(sessionId: string, title: string, isSsh: boolean): Tab {
  // wrapper 负责在 #terminal-panes 中定位；pane 才是终端实际打开的元素。
  // 分成两层是为了分屏：若终端直接开在 wrapper 上，第二窗格的包含块
  // 会变成已被缩窄的 wrapper，导致两个窗格重叠错位。
  const wrapper = document.createElement('div');
  wrapper.className = 'term-pane';
  const pane = document.createElement('div');
  pane.className = 'term-pane term-pane-primary';
  wrapper.appendChild(pane);
  panesEl.appendChild(wrapper);

  const term = new Terminal({
    cursorBlink: true,
    fontSize,
    fontFamily: FONT_STACK,
    theme: TERM_THEMES[theme],
    scrollback: 5000,
  });
  const fit = new FitAddon();
  term.loadAddon(fit);
  term.loadAddon(new WebLinksAddon((_event, uri) => {
    void invoke('open_external', { program: uri })
      .catch((e) => setStatus(t('openLinkFailed', { err: errText(e) })));
  }));

  const tab: Tab = {
    id: nextId++, title, sessionId, isSsh,
    sftpPath: '.', sftpRequest: 0, syncGroup: 0, color: '',
    term, fit, wrapper, pane, split: null, search: null, disposables: [], closed: false,
    disconnected: false,
    remote: null,
  };

  attachTerminal(term, pane);
  attachKeyHandler(term);
  tab.disposables.push(
    term.onData((data) => sendInput(tab, data)),
    term.onResize(({ cols, rows }) => {
      if (tab.closed) return;
      void invoke('pty_resize', { sessionId: tab.sessionId, cols, rows }).catch(() => {});
    }),
    term.onCursorMove(() => {
      if (currentTab() !== tab) return;
      const buf = term.buffer.active;
      setPosition(buf.cursorY + 1, buf.cursorX + 1);
    }),
  );

  tabs.push(tab);
  activeTab = tabs.length - 1;
  renderTabs();
  hideWelcome();
  setActivePanes();
  fitTab(tab);
  if (isSsh) void refreshSftp();
  saveOpenTabs();
  return tab;
}

function activateTab(index: number): void {
  activeTab = index;
  renderTabs();
  setActivePanes();
  const tab = currentTab();
  if (!tab) return;
  fitTab(tab);
  tab.term.focus();
  refreshSftpVisibility();
  if (tab.isSsh) void refreshSftp();
  saveOpenTabs();
}

/** 只显示当前标签页的窗格，避免隐藏的终端被读写。 */
function setActivePanes(): void {
  tabs.forEach((tab, i) => {
    tab.wrapper.style.display = i === activeTab ? '' : 'none';
  });
}

/**
 * 关闭标签页。
 * @param keepPane 为 true 时不刷新界面，由批量关闭的调用方统一刷新。
 */
function closeTab(tab: Tab, keepPane = false): void {
  const index = tabs.indexOf(tab);
  if (index < 0) return;

  // 先关分屏：此时 tab.split 仍在，其 onResize 回调里的守卫才有效。
  destroySplit(tab);

  // 标记关闭并立刻释放监听，避免 dispose 期间触发的 onResize 再打后端。
  tab.closed = true;
  for (const disposable of tab.disposables.splice(0)) {
    try { disposable.dispose(); } catch { /* 已释放 */ }
  }

  // 远程桌面标签：销毁 VNC/RDP 后端会话和 canvas
  if (tab.remote) {
    void closeRemote(tab.remote.sessionId, tab.remote.type).catch(() => {});
  } else {
    // 普通终端标签：关闭 PTY/SSH 会话
    void invoke('pty_close', { sessionId: tab.sessionId }).catch(() => {});
  }

  // 清理远程桌面标签的元数据
  remoteTabInfo.delete(tab.id);

  tab.term.dispose();
  tab.wrapper.remove();
  tabs.splice(index, 1);

  // 关闭当前标签之前的标签时，后面的标签整体左移一位，activeTab 必须跟着减一，
  // 否则会跳到右侧的下一个标签。
  if (index < activeTab) activeTab--;
  if (activeTab >= tabs.length) activeTab = Math.max(0, tabs.length - 1);

  if (keepPane) return;
  renderTabs();
  setActivePanes();
  refreshSftpVisibility();
  if (tabs.length === 0) {
    showWelcome();
  } else {
    // 新露出的标签可能还是旧尺寸（隐藏期间没跟着窗口缩放），必须重新 fit，
    // 否则画布被裁切、后端 PTY 行列数也与实际不符。
    fitActive();
    currentTab()?.term.focus();
  }
  saveOpenTabs();
}

function saveOpenTabs(): void {
  try {
    const snapshot = tabs.filter((tab) => tab.isSsh).map((tab) => ({ title: tab.title, color: tab.color }));
    localStorage.setItem('rustterm.openTabs', JSON.stringify(snapshot));
  } catch { /* 忽略写入失败 */ }
}

function showWelcome(): void {
  welcomeEl.classList.remove('hidden');
}

function hideWelcome(): void {
  if (welcomeEl.classList.contains('hidden')) return;
  welcomeEl.classList.add('hidden');
}

function refreshSftpVisibility(): void {
  const tab = currentTab();
  sftpPanel.classList.toggle('hidden', !tab?.isSsh);
}

/* ==========================================================================
   6. 终端尺寸
   ========================================================================== */

let fitTimer: number | undefined;

function computeDims(tab: Tab): { cols: number; rows: number } {
  try {
    const dims = tab.fit.proposeDimensions();
    const cols = dims && dims.cols > 0 ? dims.cols : 80;
    const rows = dims && dims.rows > 0 ? dims.rows : 24;
    return { cols, rows };
  } catch {
    return { cols: 80, rows: 24 };
  }
}

function fitTab(tab: Tab): void {
  // 远程桌面标签没有真实 xterm，跳过 fit
  if (tab.remote) return;
  if (tab.wrapper.style.display === 'none' || tab.wrapper.offsetParent === null) return;
  try { tab.fit.fit(); } catch { /* 布局尚未稳定，下次再试 */ }
  if (tab.split) {
    try { tab.split.fit.fit(); } catch { /* 同上 */ }
  }
}

function fitActive(): void {
  const tab = currentTab();
  if (tab) fitTab(tab);
}

/** 拖动窗口时高频触发，做一个 60ms 防抖。 */
function scheduleFit(): void {
  window.clearTimeout(fitTimer);
  fitTimer = window.setTimeout(fitActive, 60);
}

function applyFontSize(): void {
  for (const tab of tabs) {
    tab.term.options.fontSize = fontSize;
    if (tab.split) tab.split.term.options.fontSize = fontSize;
  }
  localStorage.setItem(FONT_KEY, String(fontSize));
  fitActive();
}

function applyTheme(): void {
  document.documentElement.dataset.theme = theme;
  for (const tab of tabs) {
    tab.term.options.theme = TERM_THEMES[theme];
    if (tab.split) tab.split.term.options.theme = TERM_THEMES[theme];
  }
}

function toggleTheme(): void {
  theme = theme === 'light' ? 'dark' : 'light';
  localStorage.setItem(THEME_KEY, theme);
  applyTheme();
  setStatus(theme === 'dark' ? t('statusThemeDark') : t('statusThemeLight'));
}

/* ==========================================================================
   7. 输入分发
   ========================================================================== */

function sendInput(tab: Tab, data: string): void {
  // 已关闭或后端会话已结束：直接丢弃。
  // 否则每次按键都会打一次注定失败的 pty_write，而用户看不到任何反馈。
  if (tab.closed || tab.disconnected) return;
  if (tab.remote) return;
  const bytes = encode(data);
  if (recording !== null) recording.push(...bytes);
  if (tab.syncGroup > 0) {
    for (const other of tabs) {
      if (other.syncGroup !== tab.syncGroup) continue;
      writeBytes(other.sessionId, bytes);
      if (other.split) writeBytes(other.split.sessionId, bytes);
    }
  } else {
    // 只写主会话。分屏是"两个独立会话并排"，不是镜像；
    // 在这里把输入也送给分屏会话会让左侧的每一次击键都打进右侧主机。
    writeBytes(tab.sessionId, bytes);
  }
}

function toggleMultiExec(): void {
  const tab = currentTab();
  if (!tab) { setStatus(t('statusNoActiveTab')); return; }
  if (tab.syncGroup === 0) {
    const group = Math.max(0, ...tabs.map((x) => x.syncGroup)) + 1;
    tab.syncGroup = group;
    setStatus(t('tabGroupJoined', { title: tab.title, group }));
  } else {
    tab.syncGroup = 0;
    setStatus(t('tabGroupLeft', { title: tab.title }));
  }
  renderTabs();
}

/* ==========================================================================
   8. 分屏
   ========================================================================== */

async function doSplit(): Promise<void> {
  const tab = currentTab();
  if (!tab) { setStatus(t('statusNoActiveTab')); return; }
  if (tab.split) { destroySplit(tab); setStatus(t('splitClosed')); return; }

  const input = await showPrompt('splitTitle', 'splitPrompt');
  // 弹窗期间用户可能已经把该标签关掉：此时容器已从文档移除，
  // 继续插窗格会得到一个永远不可见的窗格，且后端会多出一个无人引用的会话。
  if (input === null || tab.closed || !tabs.includes(tab)) return;

  // 先建窗格才能测出可用行列数
  const pane = createPane(tab, 'term-pane-secondary');
  tab.wrapper.classList.add('two-up');
  tab.pane.classList.add('two-up-primary');
  const second = new Terminal({
    cursorBlink: true, fontSize, fontFamily: FONT_STACK,
    theme: TERM_THEMES[theme], scrollback: 5000,
  });
  const secondFit = new FitAddon();
  second.loadAddon(secondFit);
  second.loadAddon(new WebLinksAddon((_event, uri) => {
    void invoke('open_external', { program: uri }).catch(() => {});
  }));
  attachTerminal(second, pane);
  attachKeyHandler(second);
  try { secondFit.fit(); } catch { /* 忽略 */ }
  const fallback: Tab = { ...tab, fit: secondFit };
  const dims = computeDims(fallback);

  const abortSplit = () => {
    second.dispose();
    pane.remove();
    tab.pane.classList.remove('two-up-primary');
    tab.wrapper.classList.remove('two-up');
  };
  let sessionId: string;
  try {
    if (!input.trim()) {
      sessionId = await invoke<string>('pty_spawn', { cols: dims.cols, rows: dims.rows });
    } else {
      const target = parseTarget(input);
      if (!target.user) {
        setStatus(t('splitNeedUser'));
        abortSplit();
        return;
      }
      const password = await showPrompt('sshPasswordTitle', 'sshPasswordPrompt', '', true, {
        user: target.user, host: target.host,
      });
      if (password === null) { abortSplit(); return; }
      sessionId = await invoke<string>('ssh_connect', {
        host: target.host, port: target.port, user: target.user, password,
        cols: dims.cols, rows: dims.rows,
      });
    }
  } catch (e) {
    if (tab.closed || !tabs.includes(tab)) return;
    setStatus(t('splitFailed', { err: errText(e) }));
    abortSplit();
    return;
  }

  // 弹窗/连接期间用户可能已把这个标签关掉：此时 tab 已 dispose，
  // 再挂上去会留下一个没有任何引用的远端会话和一个永不回收的终端。
  if (tab.closed || !tabs.includes(tab)) {
    void invoke('pty_close', { sessionId }).catch(() => {});
    abortSplit();
    return;
  }

  tab.split = { term: second, fit: secondFit, sessionId, pane, disposables: [] };
  tab.split.disposables.push(
    second.onData((data) => {
      const bytes = encode(data);
      if (recording !== null) recording.push(...bytes);
      writeBytes(sessionId, bytes);
    }),
    second.onResize(({ cols, rows }) => {
      if (tab.split?.sessionId === sessionId) void invoke('pty_resize', { sessionId, cols, rows }).catch(() => {});
    }),
  );

  renderTabs();
  fitTab(tab);
  second.focus();
  setStatus(t('splitOpened', { target: input.trim() || t('splitLocal') }));
}

function destroySplit(tab: Tab): void {
  const split = tab.split;
  if (!split) return;
  void invoke('pty_close', { sessionId: split.sessionId }).catch(() => {});
  // 先把 tab.split 置空并释放监听，再 dispose 终端：
  // dispose 会触发 onResize，此时守卫读到 null 便不会再发 pty_resize。
  tab.split = null;
  for (const disposable of split.disposables.splice(0)) {
    try { disposable.dispose(); } catch { /* 已释放 */ }
  }
  split.term.dispose();
  split.pane.remove();
  tab.pane.classList.remove('two-up-primary');
  tab.wrapper.classList.remove('two-up');
  if (!tab.closed) { renderTabs(); fitTab(tab); }
}

/* ==========================================================================
   9. 后端事件
   ========================================================================== */

function findTabBySession(sessionId: string): { tab: Tab; split: boolean } | null {
  for (const tab of tabs) {
    // 远程桌面会话有独立的 event 流（vnc:frame），不走 pty:data
    if (tab.remote) continue;
    if (tab.sessionId === sessionId) return { tab, split: false };
    if (tab.split?.sessionId === sessionId) return { tab, split: true };
  }
  return null;
}

void listen<{ sessionId: string; data: number[] }>('pty:data', (event) => {
  const found = findTabBySession(event.payload.sessionId);
  if (!found) return;
  // 远程桌面标签不接收 pty 数据
  if (found.tab.remote) return;
  const target = found.split && found.tab.split ? found.tab.split.term : found.tab.term;
  target.write(new Uint8Array(event.payload.data));
});

void listen<{ sessionId: string }>('pty:close', (event) => {
  const found = findTabBySession(event.payload.sessionId);
  if (!found) return;
  if (found.split) {
    destroySplit(found.tab);
    setStatus(t('tabSplitClosed'));
  } else {
    // 主会话结束：标记该标签已断开。
    // 之前只写了一句状态栏文案，标签看起来还活着，继续输入会被静默丢弃
    //（pty_write 已找不到会话），用户只会觉得"键盘没反应"。
    found.tab.disconnected = true;
    renderTabs();
    setStatus(t('tabClosed', { title: found.tab.title }));
  }
});

/** 传输收尾：清空进度条并复位记录。成功、失败、取消都要走这里。 */
let transferEpoch = 0;

function finishTransfer(): void {
  const epoch = ++transferEpoch;
  sftpProgressFill.style.width = '100%';
  setTimeout(() => {
    // 600ms 内又开始了新传输就跳过这次收尾
    if (epoch !== transferEpoch) return;
    sftpProgress.classList.add('hidden');
    sftpProgressFill.style.width = '0%';
  }, 600);
  if (!transferRunning && transferQueue.length === 0) currentTransferId = null;
  resumeTransferId = null;
}

void listen<{ sessionId: string; sent: number; total: number; label: string }>('transfer:progress', (event) => {
  transferEpoch++;   // 新进度到来，取消上面那个延迟收尾
  const { sent, total, label } = event.payload;
  const percent = total > 0 ? (sent / total) * 100 : 0;
  sftpProgress.classList.remove('hidden');
  sftpProgressLabel.textContent = `${label} ${(sent / 1024).toFixed(1)} / ${(total / 1024).toFixed(1)} KB`;
  sftpProgressFill.style.width = `${percent}%`;
});

void listen<{ sessionId: string; label: string }>('transfer:done', (event) => {
  sftpStatusEl.textContent = t('sftpTransferDone', { label: event.payload.label });
  finishTransfer();
  void refreshSftp();
});

/* ==========================================================================
   10. 查找
   ========================================================================== */

function activeSearch(): SearchAddon | null {
  const tab = currentTab();
  if (!tab) return null;
  if (!tab.search) {
    tab.search = new SearchAddon();
    tab.term.loadAddon(tab.search);
  }
  return tab.search;
}

function doFind(direction: 'next' | 'prev'): void {
  const addon = activeSearch();
  const query = findInput.value;
  if (!addon || !query) return;
  const options = { caseSensitive: false, incremental: direction === 'next' };
  const hit = direction === 'next' ? addon.findNext(query, options) : addon.findPrevious(query, options);
  if (!hit) setStatus(t('findNoMatch'));
}

function openFindBar(): void {
  findBar.classList.remove('hidden');
  findInput.focus();
  findInput.select();
}

/* ==========================================================================
   11. SFTP
   ========================================================================== */

function remoteJoin(base: string, name: string): string {
  if (!base || base === '.') return name;
  if (base === '/') return `/${name}`;
  return `${base.replace(/\/$/, '')}/${name}`;
}

function updateSftpPathDisplay(): void {
  const tab = currentTab();
  const input = $<HTMLInputElement>('sftp-path-input');
  if (tab && document.activeElement !== input) input.value = tab.sftpPath;
}

/**
 * 列出当前会话的目录。
 *
 * 用序号丢弃过期响应：快速连点目录时会有多个请求在飞，
 * 先发的可能后返回，直接渲染会让列表和地址栏停在旧目录上。
 */
async function refreshSftp(): Promise<void> {
  const tab = currentTab();
  if (!tab?.isSsh) return;
  const requestId = ++tab.sftpRequest;
  const path = tab.sftpPath;
  // 立即清空：否则在新目录返回前的这段时间里，界面（以及可双击的列表项）
  // 仍然是上一个会话/上一个目录的内容。
  sftpListEl.textContent = '';
  const pending = document.createElement('li');
  pending.className = 'empty';
  pending.textContent = '…';
  sftpListEl.appendChild(pending);
  try {
    const all = await invoke<RemoteEntry[]>('sftp_list_dir', { sessionId: tab.sessionId, path });
    if (currentTab() !== tab || requestId !== tab.sftpRequest) return;
    updateSftpPathDisplay();

    // 默认不显示隐藏文件；过滤在前端做，切换开关时不必重新列目录。
    const hiddenCount = all.filter((e) => e.hidden).length;
    const entries = showHiddenFiles ? all : all.filter((e) => !e.hidden);
    renderHiddenCount(hiddenCount);

    sftpListEl.textContent = '';
    if (entries.length === 0) {
      const li = document.createElement('li');
      li.className = 'empty';
      // 有内容但被隐藏时，给出提示而不是让人以为目录是空的
      li.textContent = all.length === 0 ? t('sftpEmptyDir') : t('sftpAllHidden');
      sftpListEl.appendChild(li);
    }
    for (const entry of entries) {
      const li = document.createElement('li');
      if (entry.is_dir) li.classList.add('dir');
      if (entry.hidden) li.classList.add('hidden-file');

      const name = document.createElement('span');
      name.className = 'name';
      // textContent：文件名来自远端，绝不能当 HTML 解析
      name.textContent = `${entry.is_dir ? '[D]' : '[F]'} ${entry.name}`;
      li.appendChild(name);

      if (!entry.is_dir) {
        const size = document.createElement('span');
        size.className = 'size';
        size.textContent = humanSize(entry.size);
        li.appendChild(size);
      }

      li.ondblclick = () => {
        if (entry.is_dir) { tab.sftpPath = entry.path; void refreshSftp(); }
        else void enqueueDownload(entry.path, entry.name, tab);
      };
      li.oncontextmenu = (ev) => { ev.preventDefault(); void showSftpMenu(tab, entry); };
      sftpListEl.appendChild(li);
    }
    sftpStatusEl.textContent = t('sftpCount', { n: entries.length });
  } catch (e) {
    // 切到别的标签后，这个错误已经不属于当前界面了
    if (currentTab() !== tab || requestId !== tab.sftpRequest) return;
    sftpStatusEl.textContent = t('sftpListFailed', { err: errText(e) });
  }
}

/** 显示被隐藏的条目数；全部可见时不占位。 */
function renderHiddenCount(hiddenCount: number): void {
  const el = $('sftp-hidden-count');
  el.textContent = !showHiddenFiles && hiddenCount > 0
    ? t('sftpHiddenCount', { n: hiddenCount })
    : '';
}

async function showSftpMenu(tab: Tab, entry: RemoteEntry): Promise<void> {
  const action = await showPrompt('sftpMenuTitle', 'sftpMenuBody', '1', false, { name: entry.name });
  // 弹窗期间标签可能已被关闭，此时不要再对已断开的会话发操作。
  if (!action || tab.closed) return;
  if (action === '1' && !entry.is_dir) void enqueueDownload(entry.path, entry.name, tab);
  else if (action === '2' && !entry.is_dir) void enqueueDownloadResume(entry.path, entry.name, tab);
  else if (action === '3') {
    const newName = await showPrompt('sftpRenameTitle', 'sftpRenamePrompt', entry.name);
    if (!newName || newName === entry.name) return;
    const slash = entry.path.lastIndexOf('/');
    // slash === 0 表示文件就在远端根目录下，此时必须拼回 "/"，
    // 否则新路径会变成相对路径，文件会被搬到远端 home 目录。
    const dir = slash <= 0 ? entry.path.slice(0, slash + 1) : entry.path.slice(0, slash);
    const newPath = dir.endsWith('/') ? `${dir}${newName}` : `${dir}/${newName}`;
    try {
      await invoke('sftp_rename', { sessionId: tab.sessionId, oldPath: entry.path, newPath });
      setStatus(t('sftpRenamed', { from: entry.name, to: newName }));
      void refreshSftp();
    } catch (e) {
      sftpStatusEl.textContent = t('sftpRenameFailed', { err: errText(e) });
    }
  } else if (action === '4') {
    if (!await showConfirm('sftpDeleteTitle', 'sftpDeleteConfirm', { name: entry.name })) return;
    try {
      await invoke('sftp_delete', { sessionId: tab.sessionId, path: entry.path, isDir: entry.is_dir });
      void refreshSftp();
    } catch (e) {
      sftpStatusEl.textContent = t('sftpDeleteFailed', { err: errText(e) });
    }
  }
}

/**
 * 加入下载队列。
 * `tab` 必须由调用方传入（即列出该文件的那个标签），
 * 不能在这里取 currentTab()：用户可能已经切到别的 SSH 会话，
 * 那样就会用 B 的会话去下载 A 的路径。
 */
async function enqueueDownload(remotePath: string, filename: string, tab: Tab): Promise<void> {
  if (tab.closed) return;
  const local = await saveDialog({ defaultPath: filename });
  if (!local || typeof local !== 'string' || tab.closed) return;
  enqueueTransfer({
    id: `t-${Date.now()}`, sessionId: tab.sessionId,
    kind: 'download', local, remote: remotePath,
    label: t('sftpDownloading', { name: filename }),
  });
}

async function enqueueDownloadResume(remotePath: string, filename: string, tab: Tab): Promise<void> {
  if (tab.closed) return;
  const local = await saveDialog({ defaultPath: filename });
  if (!local || typeof local !== 'string' || tab.closed) return;
  // 续传不走队列，用独立的 id，避免覆盖队列的 currentTransferId
  // （否则此时点"取消"会取消到另一个传输）。
  const id = `t-${Date.now()}`;
  resumeTransferId = id;
  currentTransferId = id;
  sftpStatusEl.textContent = t('sftpDownloadResumeStarted');
  // 让进度条立刻出现：否则只有等第一个 progress 事件到达才可见
  sftpProgress.classList.remove('hidden');
  sftpProgressFill.style.width = '0%';
  sftpProgressLabel.textContent = t('sftpDownloadResumeStarted');
  await invoke('sftp_download_resume', { sessionId: tab.sessionId, transferId: id, remote: remotePath, local })
    .catch((e) => {
      sftpStatusEl.textContent = t('sftpResumeFailed', { err: errText(e) });
      finishTransfer();
    })
    .finally(() => {
      if (resumeTransferId === id) resumeTransferId = null;
      if (currentTransferId === id) currentTransferId = null;
    });
}

/* --- 传输队列 --- */
function enqueueTransfer(job: TransferJob): void {
  transferQueue.push(job);
  renderTransferQueue();
  if (!transferRunning) void runNextTransfer();
}

async function runNextTransfer(): Promise<void> {
  const job = transferQueue.shift();
  renderTransferQueue();
  if (!job) { transferRunning = false; return; }
  transferRunning = true;
  currentTransferId = job.id;
  sftpStatusEl.textContent = `${job.label}…`;
  try {
    if (job.kind === 'upload') {
      await invoke('sftp_upload', { sessionId: job.sessionId, transferId: job.id, local: job.local, remote: job.remote });
    } else {
      await invoke('sftp_download', { sessionId: job.sessionId, transferId: job.id, remote: job.remote, local: job.local });
    }
    // 成功：progress 事件已经更新过进度，这里只做收尾（不经过 transfer:done 的那条路径时也能清）
  } catch (e) {
    sftpStatusEl.textContent = t('sftpTransferFailed', { label: job.label, err: errText(e) });
  }
  // 成功/失败/取消都要收尾：否则进度条会一直挂在面板底部。
  // 成功时 transfer:done 也会调一次 finishTransfer，epoch 机制保证不会互相干扰。
  finishTransfer();
  // 队列里每完成一项就刷新列表，上传完能立刻看到新文件
  if (job.kind === 'upload') void refreshSftp();
  currentTransferId = null;
  await runNextTransfer();
}

function renderTransferQueue(): void {
  $('transfer-queue').textContent = transferQueue.length === 0 ? '' : t('queueCount', { n: transferQueue.length });
}

/* ==========================================================================
   12. 批量命令
   ========================================================================== */

async function openBatchCommand(): Promise<void> {
  if (tabs.length === 0) { setStatus(t('statusNoTab')); return; }
  const list = $<HTMLUListElement>('batch-tabs');
  list.textContent = '';
  tabs.forEach((tab, i) => {
    const li = document.createElement('li');
    const box = document.createElement('input');
    box.type = 'checkbox';
    box.dataset.tabId = String(tab.id);
    box.checked = i === activeTab;
    const label = document.createElement('span');
    label.className = 'tab-label';
    label.textContent = tab.title;
    li.append(box, label);
    list.appendChild(li);
  });
  $<HTMLTextAreaElement>('batch-cmd').value = '';
  $('batch-modal').classList.remove('hidden');
  setTimeout(() => $<HTMLTextAreaElement>('batch-cmd').focus(), 30);
}

/* ==========================================================================
   13. 会话管理
   ========================================================================== */

async function loadSessions(): Promise<SavedSession[]> {
  try {
    cachedSessions = await invoke<SavedSession[]>('list_sessions');
  } catch (e) {
    setStatus(t('sessReadFailed', { err: errText(e) }));
    cachedSessions = [];
  }
  return cachedSessions;
}

async function showSessionManager(): Promise<void> {
  const list = await loadSessions();
  const panel = $('sessions-panel');
  panel.textContent = '';
  const tree = document.createElement('div');
  tree.className = 'tree';
  panel.appendChild(tree);

  if (list.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'tree-item empty';
    empty.textContent = t('treeNoSessions');
    tree.appendChild(empty);
  } else {
    const groups: Record<string, SavedSession[]> = {};
    for (const session of list) {
      const group = session.group || t('sessGroup');
      (groups[group] ??= []).push(session);
    }
    for (const [group, items] of Object.entries(groups)) {
      const folder = document.createElement('div');
      folder.className = 'tree-item folder';
      folder.textContent = `📁 ${group}`;
      tree.appendChild(folder);
      for (const session of items) {
        const proto = session.protocol ?? 'ssh';
        const item = document.createElement('div');
        item.className = 'tree-item';
        if (proto !== 'ssh') item.classList.add('remote-session');

        // 协议徽标：[SSH] / [VNC] / [RDP]
        const protoTag = proto === 'ssh'
          ? ''
          : `[${proto.toUpperCase()}] `;

        // 用 textContent 组装，会话名可能来自导入文件，不能当 HTML
        item.textContent = session.save_password
          ? `${protoTag}${session.name}  · ${t('sessPasswordMark')}`
          : `${protoTag}${session.name}`;

        if (session.color) item.style.color = session.color;

        // 单击：填入快速连接框（仅 SSH 有意义）
        item.onclick = () => {
          if (proto === 'ssh') {
            $<HTMLInputElement>('quick-input').value =
              `${session.user}@${session.host}:${session.port}`;
            setStatus(t('sessionSelected', { name: session.name }));
          } else {
            setStatus(t('sessionSelected', { name: session.name }));
          }
        };

        // 双击：连接
        item.ondblclick = () => void connectSaved(session);

        // 右键：删除
        item.oncontextmenu = async (ev) => {
          ev.preventDefault();
          if (!await showConfirm('sessDeleteTitle', 'sessDeleteConfirm', { name: session.name })) return;
          // delete_session 会一并清除凭据库里的密码
          await invoke('delete_session', {
            host: session.host, port: session.port, user: session.user,
          }).catch(() => {});
          await showSessionManager();
        };

        // 已存密码的会话额外提供"忘记密码"：只清凭据，保留会话配置
        if (session.save_password) {
          const forget = document.createElement('button');
          forget.className = 'tree-action';
          forget.textContent = t('sessClearPassword');
          forget.onclick = async (clickEvent) => {
            clickEvent.stopPropagation();
            await invoke('delete_secret', {
              host: session.host, port: session.port, user: session.user,
            }).catch(() => {});
            // 同步更新标记，否则界面会说"已存密码"但凭据已不在
            await invoke('save_session_full', {
              host: session.host, port: session.port, user: session.user,
              group: session.group, color: session.color,
              savePassword: false, password: null,
              protocol: session.protocol ?? 'ssh',
            }).catch(() => {});
            setStatus(t('sessPasswordCleared'));
            await showSessionManager();
          };
          item.appendChild(forget);
        }
        tree.appendChild(item);
      }
    }
  }
  setStatus(t('sessionsCount', { n: list.length }));
  await renderRecent();
}

/**
 * 取连接要用的密码。
 *
 * 若该会话勾选了"记住密码"，先从系统凭据库读取并作为默认值填入输入框：
 * 用户直接回车即可，但仍是显式动作——读取本身不需要额外授权，
 * 所以保留这一步让用户有机会改用别的密码。
 */
async function askPassword(user: string, host: string, port: number): Promise<string | null> {
  let remembered = '';
  try {
    remembered = (await invoke<string | null>('get_secret', { host, port, user })) ?? '';
  } catch {
    // 凭据库不可用或读取失败都不该挡住连接，退回手动输入
    remembered = '';
  }
  return showPrompt('sshPasswordTitle', 'sshPasswordPrompt', remembered, true, { user, host });
}

async function connectSaved(session: SavedSession): Promise<void> {
  const proto = session.protocol ?? 'ssh';

  // ===== 远程桌面会话（VNC / RDP / SPICE） =====
  if (proto === 'vnc' || proto === 'rdp' || proto === 'spice') {
    const isVnc = proto === 'vnc';
    const isSpice = proto === 'spice';
    const isRdp = proto === 'rdp';

    // 每种协议对应的文案 key
    const dialogTitle = isVnc ? 'vncTitle' : isSpice ? 'spiceTitle' : 'rdpTitle';
    const passwordKey = isVnc ? 'vncPasswordPrompt' : isSpice ? 'spicePasswordPrompt' : 'rdpPasswordPrompt';
    const connectingKey = isVnc ? 'vncConnecting' : isSpice ? 'spiceConnecting' : 'rdpConnecting';
    const connectedKey = isVnc ? 'vncConnected' : isSpice ? 'spiceConnected' : 'rdpConnected';
    const failedKey = isVnc ? 'vncFailed' : isSpice ? 'spiceFailed' : 'rdpFailed';
    const tabTitleKey = isVnc ? 'vncTabTitle' : isSpice ? 'spiceTabTitle' : 'rdpTabTitle';
    const tabKind: 'vnc' | 'rdp' | 'spice' = isVnc ? 'vnc' : isSpice ? 'spice' : 'rdp';

    // RDP 需要用户名，VNC / SPICE 不需要
    let username = '';
    if (isRdp) {
      username = await showPrompt(dialogTitle, 'rdpUserPrompt') ?? '';
      if (!username) return;
    }

    const password = await showPrompt(dialogTitle, passwordKey, '', true);
    if (password === null) return;

    setStatus(t(connectingKey, { host: session.host, port: session.port }));

    // 先建一个空的远程桌面标签，拿它的 wrapper 作为 canvas 容器
    const placeholder = createRemoteTab('', t(tabTitleKey, { host: session.host }), tabKind);
    remoteTabInfo.set(placeholder.id, {
      host: session.host, port: session.port, protocol: tabKind,
    });

    try {
      const sessionId = await connectRemote(
        tabKind, session.host, session.port, username, password, placeholder.wrapper,
      );
      placeholder.sessionId = sessionId;
      placeholder.remote = { sessionId, type: tabKind };

      // 如果 connectRemote 期间用户已经关掉了这个标签，立即清理后端会话
      if (placeholder.closed) {
        await closeRemote(sessionId, tabKind);
        return;
      }

      setStatus(t(connectedKey, { host: session.host }));
    } catch (e) {
      closeTab(placeholder, true);
      renderTabs();
      setActivePanes();
      if (tabs.length === 0) showWelcome();
      setStatus(t(failedKey, { err: errText(e) }));
    }
    return;
  }

  // ===== SSH 会话（原有逻辑） =====
  const password = await askPassword(session.user, session.host, session.port);
  if (password === null) return;
  await new Promise((r) => setTimeout(r, 0));
  const dims = dimsForNewTab();
  try {
    const sessionId = await invoke<string>('ssh_connect', {
      host: session.host, port: session.port, user: session.user, password,
      cols: dims.cols, rows: dims.rows,
    });
    createTab(sessionId, t('sshTabTitle', { user: session.user, host: session.host }), true);
    setStatus(t('sshConnected', { user: session.user, host: session.host }));
  } catch (e) {
    setStatus(t('sshFailed', { err: errText(e) }));
  }
}

/** 新建标签页前估算行列数：没有终端时按可用区域折算。 */
function dimsForNewTab(): { cols: number; rows: number } {
  const tab = currentTab();
  if (tab) return computeDims(tab);
  const rect = panesEl.getBoundingClientRect();
  const cellWidth = fontSize * 0.6;
  const cellHeight = fontSize * 1.2;
  return {
    cols: Math.max(20, Math.floor((rect.width - 8) / cellWidth) || 80),
    rows: Math.max(5, Math.floor(rect.height / cellHeight) || 24),
  };
}

async function renderRecent(): Promise<void> {
  const grid = document.querySelector<HTMLElement>('.recent-grid');
  if (!grid) return;
  grid.textContent = '';

  const needle = welcomeFilter.trim().toLowerCase();
  const list = needle
    ? cachedSessions.filter((s) =>
        s.name.toLowerCase().includes(needle)
        || s.host.toLowerCase().includes(needle)
        || s.user.toLowerCase().includes(needle)
        || s.group.toLowerCase().includes(needle))
    : cachedSessions;

  const empty = $('welcome-empty');
  if (list.length === 0) {
    empty.textContent = cachedSessions.length === 0 ? t('welcomeNoSessionHint') : t('welcomeSearchNoMatch');
    empty.classList.remove('hidden');
    return;
  }
  empty.classList.add('hidden');

  for (const session of list.slice(0, 12)) {
    const item = document.createElement('div');
    item.className = 'recent-item';
    item.textContent = session.name;
    item.title = `${session.user}@${session.host}:${session.port}`;
    if (session.color) item.style.color = session.color;
    item.onclick = () => {
      $<HTMLInputElement>('quick-input').value = `${session.user}@${session.host}:${session.port}`;
      setStatus(t('sessionSelected', { name: session.name }));
    };
    item.ondblclick = () => void connectSaved(session);
    grid.appendChild(item);
  }
}

/* ==========================================================================
   14. 连接
   ========================================================================== */

function parseTarget(input: string): { user: string; host: string; port: number } {
  let user = '';
  let host = input.trim();
  let port = 22;
  if (host.includes('@')) [user, host] = host.split('@');
  if (host.includes(':')) {
    const parts = host.split(':');
    host = parts[0];
    port = parseInt(parts[1], 10) || 22;
  }
  return { user, host, port };
}

async function quickConnect(): Promise<void> {
  const input = $<HTMLInputElement>('quick-input').value.trim();
  if (!input) return;
  const { user, host, port } = parseTarget(input);
  if (!user) { setStatus(t('sshFormatError')); return; }

  const method = $<HTMLSelectElement>('auth-method').value;
  const jumpRaw = $<HTMLInputElement>('jump-input').value.trim();

  // 欢迎页要等连接成功、真正有标签页可显示时再隐藏；
  // 否则用户在密码框点取消后，会只剩一块空白的终端区。
  await new Promise((r) => setTimeout(r, 0));
  const dims = dimsForNewTab();

  try {
    let sessionId: string;
    if (method === 'key') {
      const keyPath = await showPrompt('sshKeyTitle', 'sshKeyPathPrompt');
      if (!keyPath) return;
      const passphrase = await showPrompt('sshKeyTitle', 'sshKeyPassphrasePrompt', '', true);
      setStatus(t('sshConnectingKey', { user, host }));
      sessionId = await invoke<string>('ssh_connect_key', {
        host, port, user, keyPath, passphrase: passphrase || null,
        cols: dims.cols, rows: dims.rows,
      });
    } else {
      // 取消必须中止：以前写成 `|| ''`，点取消会拿空密码去连，
      // 白白等一轮认证失败再弹重试框。
      const password = await showPrompt('sshPasswordTitle', 'sshPasswordPrompt', '', true, { user, host });
      if (password === null) return;
      if (jumpRaw) {
        const jump = parseTarget(jumpRaw);
        if (!jump.user) { setStatus(t('sshFormatError')); return; }
        const jumpPassword = await showPrompt('sshJumpPasswordTitle', 'sshJumpPasswordPrompt', '', true, {
          user: jump.user, host: jump.host,
        });
        if (jumpPassword === null) return;
        setStatus(t('sshConnectingJump', { host: jump.host, target: `${user}@${host}` }));
        sessionId = await invoke<string>('ssh_connect_jump', {
          jumpHost: jump.host, jumpPort: jump.port, jumpUser: jump.user, jumpPassword,
          host, port, user, password,
          cols: dims.cols, rows: dims.rows,
        });
      } else {
        setStatus(t('sshConnecting', { user, host }));

        // 超时只是给用户一个交代，底层的 invoke 并不会被取消。
        // 因此：超时后若连接仍然成功，就把这个没人引用的会话立刻关掉，
        // 否则它会一直留在后端的会话表里（连同 shell 与 SFTP 通道）。
        let timedOut = false;
        let timer: number | undefined;
        const connecting = invoke<string>('ssh_connect', {
          host, port, user, password, cols: dims.cols, rows: dims.rows,
        }).then((id) => {
          if (timedOut) void invoke('pty_close', { sessionId: id }).catch(() => {});
          return id;
        });
        const timeout = new Promise<never>((_, reject) => {
          timer = window.setTimeout(() => {
            timedOut = true;
            reject(new Error(t('sshTimeout')));
          }, 15000);
        });
        try {
          sessionId = await Promise.race([connecting, timeout]);
        } finally {
          window.clearTimeout(timer);
        }
      }
    }
    createTab(sessionId, t('sshTabTitle', { user, host }), true);
    setStatus(t('sshConnected', { user, host }));
  } catch (e) {
    setStatus(t('sshFailed', { err: errText(e) }));
    if (await showConfirm('sshRetryTitle', 'sshRetryBody')) void quickConnect();
  }
}

async function startLocalTerminal(): Promise<void> {
  await new Promise((r) => setTimeout(r, 0));
  const dims = dimsForNewTab();
  try {
    const sessionId = await invoke<string>('pty_spawn', { cols: dims.cols, rows: dims.rows });
    createTab(sessionId, t('sshLocalTitle', { n: tabs.length + 1 }), false);
  } catch (e) {
    setStatus(t('errUnknown', { err: errText(e) }));
  }
}

async function openTelnet(): Promise<void> {
  const host = await showPrompt('telnetTitle', 'telnetHostPrompt');
  if (!host) return;
  const portStr = (await showPrompt('telnetTitle', 'telnetPortPrompt', '23')) || '23';
  try {
    const sessionId = await invoke<string>('telnet_connect', { host, port: parseInt(portStr, 10) || 23 });
    createTab(sessionId, t('telnetTabTitle', { host }), false);
    setStatus(t('telnetConnected', { host, port: portStr }));
  } catch (e) {
    setStatus(t('telnetFailed', { err: errText(e) }));
  }
}

async function pingHost(): Promise<void> {
  const host = await showPrompt('pingTitle', 'pingPrompt');
  if (!host) return;

  // 优先复用已有的本地终端，避免每次 ping 都堆一个新标签
  let tab = currentTab();
  if (!tab || tab.isSsh) {
    await startLocalTerminal();
    tab = currentTab();
    if (!tab) return;
    await new Promise((r) => setTimeout(r, 400));
  }

  const isWindows = navigator.userAgent.includes('Windows');
  const command = isWindows ? `ping -t ${host}\r` : `ping ${host}\r`;
  sendInput(tab, command);
  setStatus(t('pingStarted', { host }));
}

async function toggleFullscreen(): Promise<void> {
  try {
    isFullscreen = !isFullscreen;
    await getCurrentWindow().setFullscreen(isFullscreen);
    setStatus(isFullscreen ? t('statusFullscreenOn') : t('statusFullscreenOff'));
  } catch (e) {
    setStatus(t('statusFullscreenFailed', { err: errText(e) }));
  }
}

async function startXServer(): Promise<void> {
  let display: string;
  try {
    // 后端等待 X 协议握手完成后才返回，此时 DISPLAY 必然可用。
    display = await invoke<string>('xserver_start');
  } catch (e) {
    setStatus(t('x11StartFailed', { err: errText(e) }));
    return;
  }
  setStatus(t('statusXserverStarted', { display }));
  const tab = currentTab();
  if (tab?.isSsh && await showConfirm('x11Title', 'x11SetDisplay')) {
    try {
      await invoke('setup_x11', { sessionId: tab.sessionId });
      setStatus(t('x11DisplaySet'));
    } catch (e) {
      setStatus(t('x11SetFailed', { err: errText(e) }));
    }
  }
}

/* ==========================================================================
   15. 端口转发
   ========================================================================== */

interface TunnelRecord {
  id: string;
  local_port: number;
  remote_host: string;
  remote_port: number;
}

async function openTunnelManager(): Promise<void> {
  await refreshTunnelList();
  $('tunnel-modal').classList.remove('hidden');
}

async function refreshTunnelList(): Promise<void> {
  const list = await invoke<TunnelRecord[]>('list_tunnels').catch(() => []);
  const ul = $<HTMLUListElement>('tunnel-list');
  ul.textContent = '';
  if (list.length === 0) {
    const li = document.createElement('li');
    li.className = 'empty';
    li.textContent = t('tunnelEmpty');
    ul.appendChild(li);
    return;
  }
  for (const tunnel of list) {
    const li = document.createElement('li');
    const desc = document.createElement('span');
    desc.className = 'tunnel-desc';
    desc.textContent = `127.0.0.1:${tunnel.local_port} → ${tunnel.remote_host}:${tunnel.remote_port}`;
    const remove = document.createElement('button');
    remove.textContent = t('tunnelDelete');
    remove.onclick = async () => {
      await invoke('remove_tunnel', { id: tunnel.id }).catch(() => {});
      void refreshTunnelList();
    };
    li.append(desc, remove);
    ul.appendChild(li);
  }
}

/* ==========================================================================
   16. 包检查
   ========================================================================== */

async function showPackages(): Promise<void> {
  const items: string[] = [];
  const candidates: Array<[string, string]> = [
    ['VcXsrv', 'C:\\Program Files\\VcXsrv\\vcxsrv.exe'],
    ['Xming', 'C:\\Program Files (x86)\\Xming\\Xming.exe'],
    ['PuTTY', 'C:\\Program Files\\PuTTY\\putty.exe'],
    ['PowerShell', 'C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe'],
  ];
  for (const [name, path] of candidates) {
    try {
      const exists = await invoke<boolean>('path_exists', { path });
      if (exists) {
        // 后端返回空串表示取不到版本号（例如无版本资源的 exe）
        const version = await invoke<string>('file_version', { path }).catch(() => '');
        items.push(version ? `${name} v${version} ✓` : `${name} ✓`);
      } else {
        items.push(`${name} ✗`);
      }
    } catch {
      items.push(`${name} ?`);
    }
  }
  await showAlert('packagesTitle', items.join('\n'));
  setStatus(items.join('   |   '));
}

/* ==========================================================================
   17. 设置
   ========================================================================== */

function openSettings(): void {
  $<HTMLInputElement>('set-fontsize').value = String(fontSize);
  const tab = currentTab();
  $<HTMLInputElement>('set-scrollback').value = String(tab?.term.options.scrollback ?? 5000);
  $<HTMLInputElement>('set-cursor').checked = tab?.term.options.cursorBlink ?? true;
  $<HTMLInputElement>('set-multiexec').checked = (tab?.syncGroup ?? 0) > 0;
  $<HTMLSelectElement>('set-hostkey').value = hostKeyPolicy;
  void renderHostKeyHint();
  $('settings-modal').classList.remove('hidden');
}

interface XServerConfigEntry {
  program: string;
  args: string[];
}

/** 打开 X server 配置对话框。 */
async function openXServerConfig(): Promise<void> {
  try {
    const config = await invoke<XServerConfigEntry[]>('xserver_get_config');
    renderXServerList(config);
    $('xserver-modal').classList.remove('hidden');
  } catch (e) {
    setStatus(t('errUnknown', { err: errText(e) }));
  }
}

/** 渲染配置列表。 */
function renderXServerList(list: XServerConfigEntry[]): void {
  const container = $('xserver-list');
  container.textContent = '';
  for (const entry of list) {
    container.appendChild(makeXServerRow(entry.program, entry.args.join(' ')));
  }
}

/** 生成一行配置。 */
function makeXServerRow(program: string, args: string): HTMLElement {
  const row = document.createElement('div');
  row.className = 'xserver-row';

  const programInput = document.createElement('input');
  programInput.type = 'text';
  programInput.className = 'xserver-program';
  programInput.placeholder = t('xserverProgram');
  programInput.value = program;

  const argsInput = document.createElement('input');
  argsInput.type = 'text';
  argsInput.className = 'xserver-args';
  argsInput.placeholder = t('xserverArgs');
  argsInput.value = args;

  const removeBtn = document.createElement('button');
  removeBtn.className = 'remove-btn';
  removeBtn.textContent = '×';
  removeBtn.title = t('xserverRemove');
  removeBtn.onclick = () => row.remove();

  row.append(programInput, argsInput, removeBtn);
  return row;
}

/** 从 UI 读取配置列表。 */
function collectXServerConfig(): XServerConfigEntry[] {
  const rows = document.querySelectorAll<HTMLElement>('#xserver-list .xserver-row');
  const list: XServerConfigEntry[] = [];
  rows.forEach((row) => {
    const program = row.querySelector<HTMLInputElement>('.xserver-program')?.value.trim() ?? '';
    const argsStr = row.querySelector<HTMLInputElement>('.xserver-args')?.value.trim() ?? '';
    if (!program) return;
    const args = argsStr ? argsStr.split(/\s+/).filter(Boolean) : [];
    list.push({ program, args });
  });
  return list;
}

/** 保存 X server 配置。 */
async function saveXServerConfig(): Promise<void> {
  const list = collectXServerConfig();
  try {
    await invoke('xserver_save_config', { list });
    $('xserver-modal').classList.add('hidden');
    setStatus(t('xserverSaved'));
  } catch (e) {
    setStatus(t('errUnknown', { err: errText(e) }));
  }
}

/** 说明当前策略的含义，并显示实际使用的 known_hosts 路径。 */
async function renderHostKeyHint(): Promise<void> {
  const hint = $('hostkey-hint');
  const text = hostKeyPolicy === 'strict'
    ? t('hostKeyHintStrict')
    : hostKeyPolicy === 'insecure'
      ? t('hostKeyHintInsecure')
      : t('hostKeyHintAcceptNew');
  let path: string | null = null;
  if (hostKeyPolicy !== 'insecure') {
    path = await invoke<string | null>('known_hosts_file').catch(() => null);
  }
  hint.textContent = path
    ? `${text}\n${t('hostKeyHintPath', { path })}`
    : `${text}\n${hostKeyPolicy === 'insecure' ? '' : t('hostKeyHintNoPath')}`.trim();
}

function saveSettings(): void {
  fontSize = parseInt($<HTMLInputElement>('set-fontsize').value, 10) || 14;
  const scrollback = parseInt($<HTMLInputElement>('set-scrollback').value, 10) || 5000;
  const cursorBlink = $<HTMLInputElement>('set-cursor').checked;
  const multiExec = $<HTMLInputElement>('set-multiexec').checked;
  const policy = $<HTMLSelectElement>('set-hostkey').value as HostKeyPolicy;
  for (const tab of tabs) {
    tab.term.options.fontSize = fontSize;
    tab.term.options.scrollback = scrollback;
    tab.term.options.cursorBlink = cursorBlink;
    if (tab.split) {
      tab.split.term.options.fontSize = fontSize;
      tab.split.term.options.scrollback = scrollback;
      tab.split.term.options.cursorBlink = cursorBlink;
    }
  }
  const tab = currentTab();
  if (tab && (tab.syncGroup > 0) !== multiExec) toggleMultiExec();

  if (policy !== hostKeyPolicy) {
    hostKeyPolicy = policy;
    localStorage.setItem(HOSTKEY_KEY, hostKeyPolicy);
    void invoke('set_host_key_policy', { policy: hostKeyPolicy }).catch(() => {});
  }

  localStorage.setItem(FONT_KEY, String(fontSize));
  $('settings-modal').classList.add('hidden');
  fitActive();
  setStatus(t('settingsSaved'));
}

/* ==========================================================================
   18. 宏
   ========================================================================== */

async function toggleRecording(): Promise<void> {
  if (recording === null) {
    recording = [];
    setStatus(t('macroRecording'));
    return;
  }
  const name = await showPrompt('macroSaveTitle', 'macroNamePrompt');
  if (name) {
    recordedMacros.push({ name, data: recording });
    saveMacros();
    setStatus(t('macroSaved', { name }));
  }
  recording = null;
}

function openMacroModal(): void {
  if (recordedMacros.length === 0) { setStatus(t('macroNone')); return; }
  const list = $<HTMLUListElement>('macro-list');
  list.textContent = '';
  for (const macro of recordedMacros) {
    const li = document.createElement('li');
    const label = document.createElement('span');
    label.textContent = macro.name;
    const size = document.createElement('span');
    size.className = 'dim';
    size.textContent = t('macroBytes', { n: macro.data.length });
    li.append(label, size);
    li.onclick = () => {
      const tab = currentTab();
      if (tab) {
        writeBytes(tab.sessionId, macro.data);
        setStatus(t('macroReplayed', { name: macro.name }));
      }
      $('macro-modal').classList.add('hidden');
    };
    list.appendChild(li);
  }
  $('macro-modal').classList.remove('hidden');
}

/* ==========================================================================
   19. 语言切换
   ========================================================================== */

/** 原生菜单栏的文案在 Rust 侧，切换语言后需要让它重建一次。 */
function syncMenuLanguage(): void {
  void invoke('set_menu_language', { lang }).catch(() => { /* 不影响界面语言 */ });
}

function switchLanguage(): void {
  lang = lang === 'zh' ? 'en' : 'zh';
  saveLang(lang);
  t = makeT(lang);
  document.documentElement.dataset.lang = lang;
  applyI18n(t);
  syncMenuLanguage();

  // 已渲染的动态内容整体重绘
  renderTabs();
  renderTransferQueue();
  if (openPrompt) {
    $('prompt-title').textContent = t(openPrompt.title);
    $('prompt-message').textContent = t(openPrompt.message, openPrompt.params);
  }
  void showSessionManager();
  setStatus(t('statusLangSwitched', { lang: lang === 'zh' ? t('langNameZh') : t('langNameEn') }));
}

/* ==========================================================================
   20. 事件绑定
   ========================================================================== */

/* --- SFTP 拖放上传 ---
   Tauri v2 默认由 OS 接管外部文件拖放，WebView2 不会派发 HTML5 的 drop 事件；
   而且 WebView2 的 File 对象也没有 Electron 那种 path 属性。
   所以必须走 Tauri 的窗口级拖放事件，从 payload.paths 里拿真实路径。 */

/**
 * 上传若干本地路径（文件或目录）。
 *
 * 登记 transferId 才能被 `sftp-cancel` 取消；目录上传可能很久，
 * 进度由后端的 transfer:progress 事件驱动进度条。
 */
async function uploadPaths(tab: Tab, paths: string[]): Promise<void> {
  const target = tab.sftpPath;
  let uploadedAny = false;
  for (const localPath of paths) {
    if (tab.closed) return;
    const name = localPath.split(/[\\/]/).pop() || 'file';
    const id = `up-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
    currentTransferId = id;
    sftpStatusEl.textContent = t('sftpUploading', { name });
    try {
      await invoke('sftp_upload_path', {
        sessionId: tab.sessionId,
        local: localPath,
        remote: remoteJoin(target, name),
        transferId: id,
      });
      setStatus(t('sftpUploaded', { name }));
      uploadedAny = true;
    } catch (err) {
      sftpStatusEl.textContent = t('sftpUploadFailed', { err: errText(err) });
    } finally {
      if (currentTransferId === id) currentTransferId = null;
    }
    // 每项（无论成功、失败还是被取消）都要收尾。
    // 之前只在成功路径走到这里，取消/失败后进度条会一直挂在面板底部。
    finishTransfer();
  }
  // 每项传完就刷新一次（而不是等所有项结束）：
  // 拖入多个文件或传一个大目录时，用户能立刻看到远端出现新内容。
  if (uploadedAny) void refreshSftp();
}

function setupDragDrop(): void {
  const win = getCurrentWindow();
  // 拖放事件给的是物理像素，判断是否落在面板上要换算成 CSS 像素
  const overPanel = async (position: { x: number; y: number }): Promise<boolean> => {
    if (sftpPanel.classList.contains('hidden')) return false;
    const scale = await win.scaleFactor();
    const rect = sftpPanel.getBoundingClientRect();
    const x = position.x / scale;
    const y = position.y / scale;
    return x >= rect.left && x <= rect.right && y >= rect.top && y <= rect.bottom;
  };

  void win.onDragDropEvent((event) => {
    if (event.payload.type === 'drop') {
      // 收窄在异步回调里会失效，先把值取出来
      const { paths, position } = event.payload;
      sftpListEl.classList.remove('drop-target');
      void overPanel(position).then((inside) => {
        if (!inside) return;
        const tab = currentTab();
        if (!tab?.isSsh) { setStatus(t('sftpNoSession')); return; }
        void uploadPaths(tab, paths);
      });
    } else if (event.payload.type === 'over') {
      void overPanel(event.payload.position).then((inside) => {
        sftpListEl.classList.toggle('drop-target', inside);
      });
    } else if (event.payload.type === 'leave') {
      sftpListEl.classList.remove('drop-target');
    }
  }).catch((e) => console.warn('drag-drop 监听注册失败', e));
}

$('sftp-up').onclick = () => {
  const tab = currentTab();
  if (!tab) return;
  if (!tab.sftpPath || tab.sftpPath === '.' || tab.sftpPath === '/') return;
  const index = tab.sftpPath.lastIndexOf('/');
  tab.sftpPath = index <= 0 ? (index === 0 ? '/' : '.') : tab.sftpPath.slice(0, index);
  void refreshSftp();
};

$('sftp-refresh').onclick = () => void refreshSftp();

// 显隐隐藏文件：只重新渲染，不重新列目录（隐藏标记已随列表返回）
$<HTMLInputElement>('sftp-show-hidden').addEventListener('change', (e) => {
  showHiddenFiles = (e.target as HTMLInputElement).checked;
  localStorage.setItem(SHOW_HIDDEN_KEY, showHiddenFiles ? '1' : '0');
  void refreshSftp();
});

$('sftp-upload').onclick = async () => {
  const tab = currentTab();
  if (!tab?.isSsh) return;
  const isDir = await showConfirm('sftpUploadKindTitle', 'sftpUploadKindBody',undefined,{ ok: t('commonYes'), cancel: t('commonNo') },);
  const local = await openDialog({ multiple: false, directory: isDir });
  if (!local || typeof local !== 'string') return;
  const name = local.split(/[\\/]/).pop() || 'file';
  const remote = remoteJoin(tab.sftpPath, name);
  if (isDir) {
    // 目录上传走同一条路径：登记 id 才能取消，进度由后端事件驱动
    sftpStatusEl.textContent = t('sftpUploadDirStarted', { name });
    await uploadPaths(tab, [local]);
  } else {
    enqueueTransfer({
      id: `t-${Date.now()}`, sessionId: tab.sessionId,
      kind: 'upload', local, remote, label: t('sftpUploading', { name }),
    });
  }
};

$('sftp-mkdir').onclick = async () => {
  const tab = currentTab();
  if (!tab?.isSsh) return;
  const name = await showPrompt('sftpMkdirTitle', 'sftpMkdirPrompt');
  if (!name) return;
  try {
    await invoke('sftp_mkdir', { sessionId: tab.sessionId, path: remoteJoin(tab.sftpPath, name) });
    setStatus(t('sftpMkdirDone', { name }));
    void refreshSftp();
  } catch (e) {
    sftpStatusEl.textContent = t('sftpMkdirFailed', { err: errText(e) });
  }
};

$('sftp-cancel').onclick = async () => {
  const id = currentTransferId ?? resumeTransferId;
  if (!id) return;
  await invoke('cancel_transfer', { transferId: id }).catch(() => {});
  setStatus(t('sftpCancelRequested'));
};

$<HTMLInputElement>('sftp-path-input').addEventListener('keydown', (e) => {
  if (e.key !== 'Enter') return;
  const tab = currentTab();
  if (!tab) return;
  tab.sftpPath = $<HTMLInputElement>('sftp-path-input').value.trim() || '.';
  void refreshSftp();
});

/* --- 批量命令 --- */
$('batch-run').onclick = async () => {
  const command = $<HTMLTextAreaElement>('batch-cmd').value;
  if (!command.trim()) return;
  const selected: number[] = [];
  document.querySelectorAll<HTMLInputElement>('#batch-tabs input[type="checkbox"]').forEach((box) => {
    if (box.checked) selected.push(parseInt(box.dataset.tabId || '-1', 10));
  });
  if (selected.length === 0) { await showAlert('batchTitle', t('batchNeedTab')); return; }
  const bytes = encode(`${command}\r`);
  for (const id of selected) {
    const tab = tabs.find((x) => x.id === id);
    if (!tab) continue;
    writeBytes(tab.sessionId, bytes);
    if (tab.split) writeBytes(tab.split.sessionId, bytes);
  }
  $('batch-modal').classList.add('hidden');
  setStatus(t('batchSent', { n: selected.length }));
};
$('batch-cancel').onclick = () => $('batch-modal').classList.add('hidden');

/* --- 弹窗按钮 --- */
$('set-cancel').onclick = () => $('settings-modal').classList.add('hidden');
$('set-ok').onclick = saveSettings;
$('about-ok').onclick = () => $('about-modal').classList.add('hidden');
$('macro-cancel').onclick = () => $('macro-modal').classList.add('hidden');
$('tunnel-close').onclick = () => $('tunnel-modal').classList.add('hidden');
$('xserver-add').onclick = () => {
  const container = $('xserver-list');
  container.appendChild(makeXServerRow('', ''));
};

$('xserver-save').onclick = () => void saveXServerConfig();
$('xserver-cancel').onclick = () => $('xserver-modal').classList.add('hidden');

$('tunnel-add').onclick = async () => {
  const tab = currentTab();
  if (!tab?.isSsh) { setStatus(t('statusNeedSsh')); return; }
  const spec = await showPrompt('tunnelSpecTitle', 'tunnelSpecPrompt');
  if (!spec) return;
  const match = spec.match(/^(\d+):([^:]+):(\d+)$/);
  if (!match) { setStatus(t('tunnelFormatError')); return; }
  try {
    const port = await invoke<number>('tunnel_start', {
      sessionId: tab.sessionId,
      spec: {
        local_port: parseInt(match[1], 10),
        remote_host: match[2],
        remote_port: parseInt(match[3], 10),
      },
    });
    setStatus(t('tunnelCreated', { port, host: match[2], rport: parseInt(match[3], 10) }));
    void refreshTunnelList();
  } catch (e) {
    setStatus(t('tunnelFailed', { err: errText(e) }));
  }
};

/* --- 侧栏 --- */
document.querySelectorAll<HTMLButtonElement>('.sidebar-tab').forEach((tab) => {
  tab.onclick = () => {
    document.querySelectorAll('.sidebar-tab').forEach((x) => x.classList.remove('active'));
    tab.classList.add('active');
    document.querySelectorAll('.sidebar-panel').forEach((p) => p.classList.add('hidden'));
    $(`${tab.dataset.panel}-panel`)?.classList.remove('hidden');
  };
});

document.querySelectorAll<HTMLElement>('.tree-item[data-tool]').forEach((item) => {
  item.onclick = () => void runTool(item.dataset.tool || '');
});

async function runTool(tool: string): Promise<void> {
  switch (tool) {
    case 'ssh': $<HTMLInputElement>('quick-input').focus(); break;
    case 'sftp':
      if (currentTab()?.isSsh) sftpPanel.classList.remove('hidden');
      else setStatus(t('statusNeedSsh'));
      break;
    case 'telnet': await openTelnet(); break;
    case 'rdp': await openRdpDialog(); break;
    case 'vnc': await openVncDialog(); break;
    case 'spice': await openSpiceDialog(); break;
    default: break;
  }
}

/** 打开内置 RDP 连接对话框 */
async function openRdpDialog(): Promise<void> {
  const host = await showPrompt('rdpTitle', 'rdpHostPrompt');
  if (!host) return;

  const portStr = (await showPrompt('rdpTitle', 'rdpPortPrompt', '3389')) || '3389';
  const port = parseInt(portStr, 10) || 3389;

  const username = await showPrompt('rdpTitle', 'rdpUserPrompt');
  if (!username) return;

  const password = await showPrompt('rdpTitle', 'rdpPasswordPrompt', '', true);
  if (password === null) return;

  setStatus(t('rdpConnecting', { host, port }));

  const placeholder = createRemoteTab('', t('rdpTabTitle', { host }), 'rdp');
  remoteTabInfo.set(placeholder.id, { host, port, protocol: 'rdp' });

  try {
    const sessionId = await connectRemote(
      'rdp', host, port, username, password, placeholder.wrapper,
    );
    placeholder.sessionId = sessionId;
    placeholder.remote = { sessionId, type: 'rdp' };

    if (placeholder.closed) {
      await closeRemote(sessionId, 'rdp');
      return;
    }

    setStatus(t('rdpConnected', { host }));
  } catch (e) {
    closeTab(placeholder, true);
    renderTabs();
    setActivePanes();
    if (tabs.length === 0) showWelcome();
    setStatus(t('rdpFailed', { err: errText(e) }));
  }
}

/** 打开内置 VNC 连接对话框 */
async function openVncDialog(): Promise<void> {
  const host = await showPrompt('vncTitle', 'vncHostPrompt');
  if (!host) return;

  const portStr = (await showPrompt('vncTitle', 'vncPortPrompt', '5900')) || '5900';
  const port = parseInt(portStr, 10) || 5900;

  const password = await showPrompt('vncTitle', 'vncPasswordPrompt', '', true);
  if (password === null) return;

  setStatus(t('vncConnecting', { host, port }));

  // 先建一个空的远程桌面标签，拿它的 wrapper 作为 canvas 容器
  const placeholder = createRemoteTab('', t('vncTabTitle', { host }), 'vnc');
  remoteTabInfo.set(placeholder.id, { host, port, protocol: 'vnc' });

  try {
    const sessionId = await connectRemote(
      'vnc', host, port, '', password, placeholder.wrapper,
    );
    // 把真实 sessionId 写回 tab，并让 remote 字段指向它
    placeholder.sessionId = sessionId;
    placeholder.remote = { sessionId, type: 'vnc' };

    // 如果 connectRemote 期间用户已经关掉了这个标签，立即清理后端会话
    if (placeholder.closed) {
      await closeRemote(sessionId, 'vnc');
      return;
    }

    setStatus(t('vncConnected', { host }));
  } catch (e) {
    // 连接失败：销毁占位标签
    closeTab(placeholder, true);
    renderTabs();
    setActivePanes();
    if (tabs.length === 0) showWelcome();
    setStatus(t('vncFailed', { err: errText(e) }));
  }
}

async function openSpiceDialog(): Promise<void> {
  const host = await showPrompt('spiceTitle', 'spiceHostPrompt');
  if (!host) return;

  const portStr = (await showPrompt('spiceTitle', 'spicePortPrompt', '5930')) || '5930';
  const port = parseInt(portStr, 10) || 5930;

  const password = await showPrompt('spiceTitle', 'spicePasswordPrompt', '', true);
  if (password === null) return;

  setStatus(t('spiceConnecting', { host, port }));

  const placeholder = createRemoteTab('', t('spiceTabTitle', { host }), 'spice');
  remoteTabInfo.set(placeholder.id, { host, port, protocol: 'spice' });

  try {
    const sessionId = await connectRemote(
      'spice', host, port, '', password, placeholder.wrapper,
    );
    placeholder.sessionId = sessionId;
    placeholder.remote = { sessionId, type: 'spice' };

    if (placeholder.closed) {
      await closeRemote(sessionId, 'spice');
      return;
    }
    setStatus(t('spiceConnected', { host }));
  } catch (e) {
    closeTab(placeholder, true);
    renderTabs();
    setActivePanes();
    if (tabs.length === 0) showWelcome();
    setStatus(t('spiceFailed', { err: errText(e) }));
  }
}

/* --- 快速连接 / 欢迎页 --- */
$('btn-connect').onclick = () => void quickConnect();
$<HTMLInputElement>('quick-input').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') void quickConnect();
});
$('welcome-local').onclick = () => void startLocalTerminal();
$('welcome-recover').onclick = async () => {
  const list = await loadSessions();
  if (list.length === 0) { setStatus(t('sessNoHistory')); return; }
  $<HTMLInputElement>('quick-input').value = `${list[0].user}@${list[0].host}:${list[0].port}`;
  setStatus(t('sessRecovered', { name: list[0].name }));
};
$<HTMLInputElement>('welcome-search').addEventListener('input', (e) => {
  welcomeFilter = (e.target as HTMLInputElement).value;
  void renderRecent();
});

/* --- 粘贴 --- */
document.addEventListener('paste', (e) => {
  // 输入框里的粘贴交给浏览器默认行为
  const active = document.activeElement;
  if (active instanceof HTMLInputElement || active instanceof HTMLTextAreaElement) return;
  const tab = currentTab();
  const text = e.clipboardData?.getData('text');
  if (!tab || !text || tab.disconnected) return;
  sendInput(tab, text);
});

/* --- 终端右键菜单 --- */
panesEl.addEventListener('contextmenu', async (e) => {
  e.preventDefault();
  const tab = currentTab();
  if (!tab) return;
  // 分屏时右键可能落在右侧窗格：操作对象必须是鼠标所在的那个终端，
  // 否则"复制"会复制到左边（通常是空的）选区。
  const paneEl = (e.target as HTMLElement | null)?.closest<HTMLElement>('.term-pane');
  const target = tab.split && paneEl && tab.split.pane.contains(paneEl) ? tab.split.term : tab.term;

  const hasSelection = target.hasSelection();
  const action = await showPrompt('termMenuTitle', 'termMenuBody', '', false, { hasSelection });
  if (!action) return;
  if (hasSelection && action === '1') {
    try {
      await navigator.clipboard.writeText(target.getSelection());
      setStatus(t('statusCopyMenuCopied'));
    } catch (err) {
      setStatus(t('statusCopyFailed', { err: errText(err) }));
    }
  } else if ((hasSelection && action === '2') || (!hasSelection && action === '1')) {
    try {
      const text = await navigator.clipboard.readText();
      if (text) { sendInput(tab, text); setStatus(t('statusPasted')); }
    } catch (err) {
      setStatus(t('statusPasteFailed', { err: errText(err) }));
    }
  } else if ((hasSelection && action === '3') || (!hasSelection && action === '2')) {
    target.clear();
    setStatus(t('statusCleared'));
  } else if ((hasSelection && action === '4') || (!hasSelection && action === '3')) {
    target.selectAll();
    setStatus(t('statusSelectedAll'));
  }
});

async function openAbout(): Promise<void> {
  const version = await getVersion();
  $('about-version').textContent = `RustTerm v${version}`;
  $('about-modal').classList.remove('hidden');
}

/* --- 查找栏 --- */
findInput.addEventListener('input', () => doFind('next'));
$('find-prev').onclick = () => doFind('prev');
$('find-next').onclick = () => doFind('next');
$('find-close').onclick = () => {
  findBar.classList.add('hidden');
  currentTab()?.term.focus();
};

/* --- 工具栏 --- */
document.querySelectorAll<HTMLButtonElement>('.tool-btn').forEach((btn) => {
  btn.onclick = async () => {
    switch (btn.dataset.action) {
      case 'new-local': await startLocalTerminal(); break;
      case 'new-ssh': $<HTMLInputElement>('quick-input').focus(); break;
      case 'settings': openSettings(); break;
      case 'help': await openAbout(); break;
      case 'xserver': await startXServer(); break;
      case 'multiexec': toggleMultiExec(); break;
      case 'batch': await openBatchCommand(); break;
      case 'tunneling': await openTunnelManager(); break;
      case 'telnet': await openTelnet(); break;
      case 'ping': await pingHost(); break;
      case 'split': await doSplit(); break;
      case 'find': openFindBar(); break;
      case 'lang': switchLanguage(); break;
      case 'theme': toggleTheme(); break;
      case 'ai': openAiPanel(); break;
      case 'exit': await invoke('exit_app'); break;
      default: break;
    }
  };
});

/** 关闭当前可见的模态框（Esc 用）。 */
function closeTopModal(): void {
  const modal = document.querySelector<HTMLElement>('.modal:not(.hidden)');
  if (!modal) return;
  // 模态框里的取消/关闭按钮就是"用户放弃"的语义，直接点它即可。
  const closer = modal.querySelector<HTMLButtonElement>(
    '#prompt-cancel, #confirm-cancel, #alert-ok, #batch-cancel, #macro-cancel, #tunnel-close, #about-ok, #set-cancel, #xserver-cancel, #ai-config-cancel',
  );
  if (closer) closer.click();
  else modal.classList.add('hidden');
}

/* --- 快捷键 --- */
document.addEventListener('keydown', (e) => {
  // 有模态框时，键盘属于那个模态框：在密码框里按 Ctrl+W 不该关掉底下的标签，
  // 也不能让 F11/Ctrl+L 之类穿透过去。Escape 交给模态框自己处理（见 closeTopModal）。
  if (document.querySelector('.modal:not(.hidden)')) {
    if (e.key === 'Escape') { e.preventDefault(); closeTopModal(); }
    return;
  }
  // 按住不放会连续触发，关闭标签之类的动作不该重复执行。
  if (e.repeat) return;

  if (e.key === 'F11') { e.preventDefault(); void toggleFullscreen(); return; }
  if (e.key === 'Escape') { findBar.classList.add('hidden'); return; }
  if (e.ctrlKey && e.key === 'Tab' && tabs.length > 1) {
    e.preventDefault();
    activateTab((activeTab + 1) % tabs.length);
    return;
  }
  if (!e.ctrlKey) return;
  const key = e.key.toLowerCase();
  if (key === 't') { e.preventDefault(); void startLocalTerminal(); }
  else if (key === 'n') { e.preventDefault(); $<HTMLInputElement>('quick-input').focus(); }
  else if (key === 'w') { e.preventDefault(); const tab = currentTab(); if (tab) closeTab(tab); }
  else if (key === 'l') { e.preventDefault(); void showSessionManager(); }
  else if (key === 'b') { e.preventDefault(); $('sidebar').classList.toggle('hidden'); scheduleFit(); }
  else if (key === 'f') { e.preventDefault(); openFindBar(); }
  else if (key === 'd') { e.preventDefault(); toggleTheme(); }
});

/* --- 原生菜单 --- */
void listen<string>('menu:action', async (event) => {
  const action = event.payload;
  const tab = currentTab();
  switch (action) {
    case 'term-new-local': await startLocalTerminal(); break;
    case 'term-new-ssh': $<HTMLInputElement>('quick-input').focus(); break;
    case 'term-close-tab': if (tab) closeTab(tab); break;
    case 'sess-save': await saveCurrentSession(); break;
    case 'sess-library': await showSessionManager(); break;
    case 'sess-import-putty':
      try {
        await invoke('import_putty_sessions');
        await showSessionManager();
        setStatus(t('sessImportedPutty'));
      } catch (e) { setStatus(t('sessImportPuttyFailed', { err: errText(e) })); }
      break;
    case 'sess-export': {
      const path = await saveDialog({ defaultPath: 'sessions.json', filters: [{ name: 'JSON', extensions: ['json'] }] });
      if (!path || typeof path !== 'string') break;
      try { await invoke('export_sessions', { path }); setStatus(t('sessExported')); }
      catch (e) { setStatus(t('sessExportFailed', { err: errText(e) })); }
      break;
    }
    case 'sess-import': {
      const path = await openDialog({ filters: [{ name: 'JSON', extensions: ['json'] }] });
      if (!path || typeof path !== 'string') break;
      try {
        const count = await invoke<number>('import_sessions', { path });
        await showSessionManager();
        setStatus(t('sessImported', { n: count }));
      } catch (e) { setStatus(t('sessImportFailed', { err: errText(e) })); }
      break;
    }
    case 'view-sidebar': $('sidebar').classList.toggle('hidden'); scheduleFit(); break;
    case 'view-sftp': sftpPanel.classList.toggle('hidden'); scheduleFit(); break;
    case 'view-zoom-in': fontSize = Math.min(fontSize + 1, 32); applyFontSize(); break;
    case 'view-zoom-out': fontSize = Math.max(fontSize - 1, 8); applyFontSize(); break;
    case 'view-zoom-reset': fontSize = 14; applyFontSize(); break;
    case 'view-fullscreen': await toggleFullscreen(); break;
    case 'view-reset-layout':
      $('sidebar').classList.remove('hidden');
      sftpPanel.classList.add('hidden');
      fontSize = 14;
      applyFontSize();
      setStatus(t('statusLayoutReset'));
      break;
    case 'x-start': await startXServer(); break;
    case 'x-stop':
      await invoke('xserver_stop')
        .then(() => setStatus(t('statusXserverStopped')))
        .catch((e) => setStatus(errText(e)));
      break;
    case 'tools-ssh': $<HTMLInputElement>('quick-input').focus(); break;
    case 'tools-sftp': await runTool('sftp'); break;
    case 'tools-telnet': await openTelnet(); break;
    case 'tools-rdp': await openRdpDialog(); break;
    case 'tools-vnc': await openVncDialog(); break;
    case 'tools-spice': await openSpiceDialog(); break;
    case 'tools-ping': await pingHost(); break;
    case 'tools-portscan': await openPortScan(); break;
    case 'tools-packages': await showPackages(); break;
    case 'help-about': await openAbout(); break;
    case 'settings-open': openSettings(); break;
    case 'xserver-config': await openXServerConfig(); break;
    case 'macro-record': await toggleRecording(); break;
    case 'macro-play': openMacroModal(); break;
    default: break;
  }
});

/**
 * 保存当前远程桌面标签（VNC/RDP）为会话。
 *
 * 和 saveCurrentSession 的区别：
 * - 从 tab.remote 拿 host/port，不依赖 #quick-input
 * - 不询问密码（VNC 密码存到 sessions.json 不安全；RDP 暂不支持存密码）
 */
async function saveRemoteSession(tab: Tab): Promise<void> {
  if (!tab.remote) return;

  // 从 tab.title 反解 host/port 不行（title 可能是自定义的），
  // 所以要在 tab 上记录原始的 host/port。
  const info = remoteTabInfo.get(tab.id);
  if (!info) { setStatus(t('sessNeedRemoteInfo')); return; }

  const group = (await showPrompt('sessSaveTitle', 'sessGroupPrompt')) || '';
  const color = (await showPrompt('sessSaveTitle', 'sessColorPrompt')) || '';

  try {
    await invoke('save_session_full', {
      host: info.host,
      port: info.port,
      user: '',                       // VNC/RDP 没有 user，留空
      group,
      color,
      savePassword: false,            // 远程桌面不存密码
      password: null,
      protocol: info.protocol,        // 新增参数
    });
    await showSessionManager();
    setStatus(t('sessSaved'));
  } catch (e) {
    setStatus(t('sessSaveFailed', { err: errText(e) }));
  }
}

async function saveCurrentSession(): Promise<void> {
  // 先看当前标签是不是远程桌面
  const tab = currentTab();
  if (tab?.remote) {
    await saveRemoteSession(tab);
    return;
  }
  const parsed = parseTarget($<HTMLInputElement>('quick-input').value);
  if (!parsed.user) { setStatus(t('sessNeedQuickInput')); return; }
  const group = (await showPrompt('sessSaveTitle', 'sessGroupPrompt')) || '';

  // 记住密码是可选项：凭据库不可用时直接跳过，不打扰用户
  const store = await secretStoreStatus();
  let savePassword = false;
  let password: string | null = null;
  if (store.available) {
    savePassword = await showConfirm('sessSaveTitle', 'sessRememberPasswordAsk');
    if (savePassword) {
      password = await askPassword(parsed.user, parsed.host, parsed.port);
      if (password === null) return;
    }
  } else {
    setStatus(t('secretStoreUnavailable', { detail: store.detail ?? '' }));
  }

  const color = (await showPrompt('sessSaveTitle', 'sessColorPrompt')) || '';
  try {
    await invoke('save_session_full', {
      host: parsed.host, port: parsed.port, user: parsed.user,
      group, color, savePassword, password,
    });
    await showSessionManager();
    setStatus(savePassword ? t('sessPasswordSaved') : t('sessSaved'));
  } catch (e) {
    setStatus(t('sessSaveFailed', { err: errText(e) }));
  }
}

interface SecretStoreStatus {
  available: boolean;
  detail: string | null;
}

/** 查询系统凭据库是否可用。结果缓存，避免每次保存都问一次。 */
let cachedSecretStatus: SecretStoreStatus | null = null;
async function secretStoreStatus(): Promise<SecretStoreStatus> {
  if (!cachedSecretStatus) {
    cachedSecretStatus = await invoke<SecretStoreStatus>('secret_store_status')
      .catch(() => ({ available: false, detail: 'unknown' }));
  }
  return cachedSecretStatus;
}

/* ==========================================================================
   21. AI助手
   ========================================================================== */
interface AiMessage {
  role: 'user' | 'assistant';
  content: string;
}

interface AiConfig {
  provider: string;
  api_key: string;
  base_url: string;
  model: string;
  max_tokens: number;
  temperature: number;
  read_only: boolean;
  max_history: number;
  context_lines: number;
  context_max_line_len: number;
}

let aiHistory: AiMessage[] = [];

interface AgentState {
  running: boolean;
  step: number;
  maxSteps: number;
  goal: string;
}

let agentState: AgentState | null = null;

function openAiPanel(): void {
  $('ai-panel').classList.remove('hidden');
  $<HTMLTextAreaElement>('ai-input').focus();
}

function closeAiPanel(): void {
  $('ai-panel').classList.add('hidden');
}

function loadAiConfig(): AiConfig {
  try {
    const raw = localStorage.getItem('rustterm.aiConfig');
    if (raw) {
      const p = JSON.parse(raw);
      return {
        provider: p.provider ?? 'openai',
        api_key: p.api_key ?? '',
        base_url: p.base_url ?? 'https://api.openai.com/v1',
        model: p.model ?? 'gpt-4o-mini',
        max_tokens: p.max_tokens ?? 2048,
        temperature: p.temperature ?? 0.3,
        read_only: p.read_only ?? false,
        max_history: p.max_history ?? 20,
        context_lines: p.context_lines ?? 50,
        context_max_line_len: p.context_max_line_len ?? 200,
      };
    }
  } catch { /* 忽略 */ }
  return {
    provider: 'openai',
    api_key: '',
    base_url: 'https://api.openai.com/v1',
    model: 'gpt-4o-mini',
    max_tokens: 2048,
    temperature: 0.3,
    read_only: false,
    max_history: 20,
    context_lines: 50,
    context_max_line_len: 200,
  };
}

function saveAiConfig(cfg: AiConfig): void {
  localStorage.setItem('rustterm.aiConfig', JSON.stringify(cfg));
}

function openAiConfig(): void {
  const cfg = loadAiConfig();
  $<HTMLSelectElement>('ai-provider').value = cfg.provider;
  $<HTMLInputElement>('ai-api-key').value = cfg.api_key;
  $<HTMLInputElement>('ai-base-url').value = cfg.base_url;
  $<HTMLInputElement>('ai-model').value = cfg.model;
  $<HTMLInputElement>('ai-max-tokens').value = String(cfg.max_tokens);
  $<HTMLInputElement>('ai-temperature').value = String(cfg.temperature);
  $<HTMLInputElement>('ai-readonly').checked = cfg.read_only;
  $<HTMLInputElement>('ai-max-history').value = String(cfg.max_history);
  $<HTMLInputElement>('ai-context-lines').value = String(cfg.context_lines);
  $<HTMLInputElement>('ai-context-max-line-len').value = String(cfg.context_max_line_len);
  $('ai-config-modal').classList.remove('hidden');
}

function readTerminalContext(tab: Tab, lines = 50, maxLineLen = 200): string {
  const buf = tab.term.buffer.active;
  const total = buf.length;
  const start = Math.max(0, total - lines);
  const out: string[] = [];
  for (let i = start; i < total; i++) {
    const line = buf.getLine(i);
    if (line) {
      let s = line.translateToString(true);
      if (s.length > maxLineLen) s = s.slice(0, maxLineLen) + '…';
      out.push(s);
    }
  }
  return out.join('\n');
}

async function sendAiMessage(): Promise<void> {
  const input = $<HTMLTextAreaElement>('ai-input');
  const text = input.value.trim();
  if (!text) return;
  input.value = '';

  const agentMode = $<HTMLInputElement>('ai-agent-mode').checked;

  if (agentMode) {
    // Agent 模式：用户输入的是"目标"
    await runAgent(text);
  } else {
    // Ask 模式：一问一答
    await askOnce(text);
  }
}

/** Ask 模式：问一次，答一次。 */
async function askOnce(text: string): Promise<void> {
  const cfg = loadAiConfig();
  const tab = currentTab();
  const ctx = tab
    ? readTerminalContext(tab, cfg.context_lines, cfg.context_max_line_len)
    : '';
  const userMsg: AiMessage = {
    role: 'user',
    content: ctx ? `[终端最近输出]\n\`\`\`\n${ctx}\n\`\`\`\n\n${text}` : text,
  };
  pushAiMessage(userMsg, cfg.max_history);
  renderAiMessages();

  try {
    const reply = await invoke<string>('ai_chat', {
      config: cfg,
      history: aiHistory.map((m) => ({ role: m.role, content: m.content })),
    });
    pushAiMessage({ role: 'assistant', content: reply }, cfg.max_history);
    renderAiMessages();

    const commands = await invoke<string[]>('ai_extract_commands', { reply });
    for (const cmd of commands) {
      const dangerous = await invoke<boolean>('ai_is_dangerous', { cmd });
      const isWrite = await invoke<boolean>('ai_is_write_command', { cmd });
      renderAiCommandCard(cmd, dangerous, isWrite, cfg.read_only);
    }
  } catch (e) {
    pushAiMessage(
      { role: 'assistant', content: t('aiError', { err: errText(e) }) },
      cfg.max_history,
    );
    renderAiMessages();
  }
}

/** Agent 模式：多步循环。 */
async function runAgent(goal: string): Promise<void> {
  const cfg = loadAiConfig();
  agentState = { running: true, step: 0, maxSteps: 10, goal };
  $('ai-stop').classList.remove('hidden');
  updateAgentStatus();

  // 把目标作为第一条 user 消息，加 [Agent] 前缀，让 AI 知道进入 Agent 模式
  pushAiMessage(
    { role: 'user', content: `[Agent] ${goal}` },
    cfg.max_history,
  );
  renderAiMessages();

  while (agentState.running && agentState.step < agentState.maxSteps) {
    agentState.step++;
    updateAgentStatus();

    // 1. 调 AI
    let reply: string;
    try {
      reply = await invoke<string>('ai_chat', {
        config: cfg,
        history: aiHistory.map((m) => ({ role: m.role, content: m.content })),
      });
    } catch (e) {
      pushAiMessage(
        { role: 'assistant', content: t('aiError', { err: errText(e) }) },
        cfg.max_history,
      );
      renderAiMessages();
      break;
    }

    pushAiMessage({ role: 'assistant', content: reply }, cfg.max_history);
    renderAiMessages();

    // 2. 检查任务是否完成
    const done = await invoke<boolean>('ai_is_task_done', { reply });
    if (done) {
      setStatus(t('aiAgentDone'));
      break;
    }

    // 3. 提取命令
    const commands = await invoke<string[]>('ai_extract_commands', { reply });
    if (commands.length === 0) {
      setStatus(t('aiAgentNoCommands'));
      break;
    }

    // 4. 逐条执行，把输出反馈给 AI
    for (const cmd of commands) {
      if (!agentState.running) break;

      const dangerous = await invoke<boolean>('ai_is_dangerous', { cmd });
      const isWrite = await invoke<boolean>('ai_is_write_command', { cmd });

      // 只读模式：跳过写命令，把"被拒绝"作为输出反馈
      if (cfg.read_only && isWrite) {
        pushAiMessage(
          {
            role: 'user',
            content: `[命令被拒绝（只读模式）]\n\`\`\`\n${cmd}\n\`\`\``,
          },
          cfg.max_history,
        );
        renderAiMessages();
        continue;
      }

      // 危险命令：弹审批
      if (dangerous) {
        const ok = await showConfirm('aiTitle', 'aiAgentApprove', { cmd });
        if (!ok) {
          pushAiMessage(
            {
              role: 'user',
              content: `[用户拒绝了危险命令]\n\`\`\`\n${cmd}\n\`\`\``,
            },
            cfg.max_history,
          );
          renderAiMessages();
          continue;
        }
      }

      // 渲染命令卡片（Agent 模式下按钮自动禁用，用户不能手动点）
      renderAgentCommandCard(cmd);

      // 执行并捕获输出
      const output = await executeAndCapture(cmd);
      pushAiMessage(
        {
          role: 'user',
          content: `[命令输出]\n\`\`\`\n${output}\n\`\`\``,
        },
        cfg.max_history,
      );
      renderAiMessages();
    }
  }

  // 循环结束
  if (agentState.step >= agentState.maxSteps) {
    setStatus(t('aiAgentMaxSteps', { max: agentState.maxSteps }));
  }

  agentState = null;
  $('ai-stop').classList.add('hidden');
  $('ai-agent-status').classList.add('hidden');
}

/** 更新 Agent 状态栏。 */
function updateAgentStatus(): void {
  const el = $('ai-agent-status');
  if (!agentState) {
    el.classList.add('hidden');
    return;
  }
  el.classList.remove('hidden');
  el.textContent = t('aiAgentRunning', {
    step: agentState.step,
    max: agentState.maxSteps,
  });
}

/** 渲染 Agent 模式下的命令卡片（只展示，不手动执行）。 */
function renderAgentCommandCard(cmd: string): void {
  const container = $('ai-messages');
  const card = document.createElement('div');
  card.className = 'ai-cmd-card';

  const pre = document.createElement('pre');
  pre.textContent = cmd;
  card.appendChild(pre);

  const badge = document.createElement('div');
  badge.className = 'ai-cmd-warn';
  badge.textContent = 'Agent 执行中…';
  card.appendChild(badge);

  container.appendChild(card);
  container.scrollTop = container.scrollHeight;
}

/**
 * 执行命令并捕获输出。
 *
 * 策略：
 * 1. 在命令末尾加一个唯一标记（echo __RUSTTERM_DONE__）。
 * 2. 监听终端输出，看到标记就认为命令结束。
 * 3. 超时兜底（默认 15 秒）。
 */
function executeAndCapture(cmd: string, timeoutMs = 15000): Promise<string> {
  return new Promise((resolve) => {
    const tab = currentTab();
    if (!tab || tab.disconnected) {
      resolve('(无活动终端)');
      return;
    }

    const marker = `__RUSTTERM_DONE_${Date.now()}__`;
    let output = '';
    let done = false;

    // 监听终端输出
    const disposable = tab.term.onData((data) => {
      if (done) return;
      output += data;
      if (output.includes(marker)) {
        done = true;
        disposable.dispose();
        clearTimeout(timer);
        // 去掉标记本身和提示符残留
        const idx = output.indexOf(marker);
        resolve(output.slice(0, idx).trim());
      }
    });

    // 超时兜底
    const timer = setTimeout(() => {
      if (done) return;
      done = true;
      disposable.dispose();
      resolve(output.trim() || '(命令超时，无输出)');
    }, timeoutMs);

    // 发送命令 + 标记
    // 用 ; 或 && 取决于平台；这里用 ; 保证标记一定执行
    const wrapped = `${cmd} ; echo ${marker}\r`;
    sendInput(tab, wrapped);
  });
}

/** 加消息并按 maxHistory 截断。 */
function pushAiMessage(msg: AiMessage, maxHistory: number): void {
  aiHistory.push(msg);
  if (aiHistory.length > maxHistory) {
    aiHistory = aiHistory.slice(-maxHistory);
  }
}

function renderAiMessages(): void {
  const container = $('ai-messages');
  container.textContent = '';
  for (const m of aiHistory) {
    const div = document.createElement('div');
    div.className = `ai-msg ai-msg-${m.role}`;
    div.textContent = m.content;
    container.appendChild(div);
  }
  container.scrollTop = container.scrollHeight;
}

function renderAiCommandCard(
  cmd: string,
  dangerous: boolean,
  isWrite: boolean,
  readOnly: boolean,
): void {
  const container = $('ai-messages');
  const card = document.createElement('div');
  card.className = 'ai-cmd-card' + (dangerous ? ' dangerous' : '');

  const pre = document.createElement('pre');
  pre.textContent = cmd;
  card.appendChild(pre);

  if (dangerous) {
    const warn = document.createElement('div');
    warn.className = 'ai-cmd-warn';
    warn.textContent = '⚠️ 危险命令，请确认后执行';
    card.appendChild(warn);
  }

  const runBtn = document.createElement('button');
  runBtn.className = 'primary';
  runBtn.textContent = '执行';

  // 只读模式：写命令禁用
  if (readOnly && isWrite) {
    const warn = document.createElement('div');
    warn.className = 'ai-cmd-warn';
    warn.textContent = `⚠️ ${t('aiReadOnlyBlocked')}`;
    card.appendChild(warn);
    runBtn.disabled = true;
    runBtn.textContent = t('aiReadOnlyBlocked');
  } else {
    runBtn.onclick = async () => {
      const tab = currentTab();
      if (!tab || tab.disconnected) return;
      if (dangerous) {
        if (!await showConfirm('aiTitle', 'aiDangerConfirm', { cmd })) return;
      }
      sendInput(tab, cmd + '\r');
      runBtn.disabled = true;
      runBtn.textContent = t('aiExecuted');
      card.classList.add('executed');
    };
  }

  card.appendChild(runBtn);
  container.appendChild(card);
  container.scrollTop = container.scrollHeight;
}

$('ai-close').onclick = closeAiPanel;
$('ai-stop').onclick = () => {
  if (agentState) {
    agentState.running = false;
    setStatus(t('aiAgentStopped'));
  }
};
$('ai-clear').onclick = () => {
  if (agentState) {
    agentState.running = false;
    agentState = null;
  }
  aiHistory = [];
  renderAiMessages();
  $('ai-stop').classList.add('hidden');
  $('ai-agent-status').classList.add('hidden');
};
$('ai-settings').onclick = openAiConfig;
$('ai-send').onclick = () => void sendAiMessage();
$<HTMLTextAreaElement>('ai-input').addEventListener('keydown', (e) => {
  if (e.key === 'Enter' && !e.shiftKey) {
    e.preventDefault();
    void sendAiMessage();
  }
});

$('ai-config-cancel').onclick = () => $('ai-config-modal').classList.add('hidden');
$('ai-config-save').onclick = () => {
  saveAiConfig({
    provider: $<HTMLSelectElement>('ai-provider').value,
    api_key: $<HTMLInputElement>('ai-api-key').value,
    base_url: $<HTMLInputElement>('ai-base-url').value,
    model: $<HTMLInputElement>('ai-model').value,
    max_tokens: parseInt($<HTMLInputElement>('ai-max-tokens').value, 10) || 2048,
    temperature: parseFloat($<HTMLInputElement>('ai-temperature').value) || 0.3,
    read_only: $<HTMLInputElement>('ai-readonly').checked,
    max_history: parseInt($<HTMLInputElement>('ai-max-history').value, 10) || 20,
    context_lines: parseInt($<HTMLInputElement>('ai-context-lines').value, 10) || 50,
    context_max_line_len: parseInt($<HTMLInputElement>('ai-context-max-line-len').value, 10) || 200,
  });
  $('ai-config-modal').classList.add('hidden');
  setStatus('AI 配置已保存');
};

/* ==========================================================================
   22. 端口扫描
   ========================================================================== */
let currentScanId: string | null = null;

async function openPortScan(): Promise<void> {
  // 默认填上当前活动 SSH 会话的主机，方便直接扫
  const tab = currentTab();
  const targets = $<HTMLInputElement>('scan-targets');
  const ports = $<HTMLInputElement>('scan-ports');
  if (tab?.isSsh && !targets.value) {
    // 从标题里粗略提取 host（"SSH:user@host"）
    const m = tab.title.match(/@([^:\s]+)/);
    if (m) targets.value = m[1];
  }
  if (!ports.value) ports.value = '22,80,443,3389,8080';
  $('scan-results').textContent = '';
  $('scan-status').textContent = '';
  $('scan-progress-fill').style.width = '0%';
  $('scan-modal').classList.remove('hidden');
}

$('scan-run').onclick = async () => {
  console.log('=== scan-run clicked ===');

  const targetsEl = $<HTMLInputElement>('scan-targets');
  const portsEl = $<HTMLInputElement>('scan-ports');
  console.log('targetsEl =', targetsEl, 'value =', JSON.stringify(targetsEl?.value));
  console.log('portsEl =', portsEl, 'value =', JSON.stringify(portsEl?.value));

  const targets = targetsEl.value.trim();
  const ports = portsEl.value.trim();
  console.log('after trim: targets =', JSON.stringify(targets), 'ports =', JSON.stringify(ports));

  const concurrency = parseInt($<HTMLInputElement>('scan-concurrency').value, 10) || 256;
  const timeout = parseInt($<HTMLInputElement>('scan-timeout').value, 10) || 800;

  if (!targets) { $('scan-status').textContent = t('scanInvalidTarget'); return; }
  if (!ports) { $('scan-status').textContent = t('scanInvalidPorts'); return; }

  const scanId = `scan-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
  currentScanId = scanId;
  $('scan-results').textContent = '';
  $('scan-status').textContent = '';
  $('scan-progress-fill').style.width = '0%';
  console.log('invoking port_scan, scanId =', scanId);

  try {
    await invoke('port_scan', { scanId, targets, ports, concurrency, timeoutMs: timeout });
    console.log('port_scan returned');
  } catch (e) {
    console.error('port_scan error:', e);
    $('scan-status').textContent = errText(e);
  }
};

$('scan-stop').onclick = async () => {
  if (!currentScanId) return;
  await invoke('cancel_scan', { scanId: currentScanId }).catch(() => {});
  $('scan-status').textContent = t('scanStopped');
};

$('scan-close').onclick = () => {
  $('scan-modal').classList.add('hidden');
};

interface ScanProgressPayload {
  scanId: string;
  scanned: number;
  total: number;
  open: Array<{ ip: string; port: number; latency_ms: number }>;
}

void listen<ScanProgressPayload>('scan:progress', (event) => {
  const { scanId, scanned, total, open } = event.payload;

  console.log('[scan] event scanId=', scanId, 'current=', currentScanId);

  if (currentScanId !== null && scanId !== currentScanId) {
    console.log('[scan] DISCARDED');
    return;
  }

  console.log('[scan] RENDERING, scanned=', scanned, '/', total, 'open=', open.length);

  const ul = document.getElementById('scan-results');
  console.log('[scan] ul =', ul);
  if (!ul) {
    console.error('[scan] scan-results not found!');
    return;
  }

  const fill = document.getElementById('scan-progress-fill');
  const status = document.getElementById('scan-status');

  const percent = total > 0 ? (scanned / total) * 100 : 0;
  if (fill) fill.style.width = `${percent}%`;
  if (status) status.textContent = `${scanned} / ${total}, open ${open.length}`;

  ul.textContent = '';
  for (const r of open) {
    const li = document.createElement('li');
    li.textContent = `${r.ip}:${r.port}`;
    ul.appendChild(li);
  }
});

void listen<{ scanId: string }>('scan:done', () => {
  // 扫描完成事件，可以在这里做一次收尾（如禁掉"停止"按钮）
});

/* ==========================================================================
   23. 启动
   ========================================================================== */

window.addEventListener('error', (e) => setStatus(`JS: ${e.message}`));
window.addEventListener('unhandledrejection', (e) => setStatus(`Promise: ${String(e.reason)}`));
window.addEventListener('resize', scheduleFit);

document.documentElement.dataset.lang = lang;
applyI18n(t);
applyTheme();
syncMenuLanguage();
// 恢复 SFTP 隐藏文件开关（缺省不显示）
$<HTMLInputElement>('sftp-show-hidden').checked = showHiddenFiles;
// 把本地保存的主机密钥策略同步给后端，默认 accept-new
void invoke('set_host_key_policy', { policy: hostKeyPolicy }).catch(() => {});
setupDragDrop();
showWelcome();

void showSessionManager();
void refreshTunnelList();

// 上次退出时留下的 SSH 标签只提示，不自动重连（需要密码）
try {
  const saved = localStorage.getItem('rustterm.openTabs');
  if (saved) {
    const list = JSON.parse(saved);
    if (Array.isArray(list) && list.length > 0) setStatus(t('sessionRestoreHint', { n: list.length }));
  }
} catch { /* 忽略损坏的存档 */ }