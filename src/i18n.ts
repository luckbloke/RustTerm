/**
 * 界面文案与国际化。
 *
 * 所有面向用户的字符串都必须经过 t()，不要在别处硬编码中文或英文。
 * static 区块由 applyI18n() 应用到 index.html 上带 data-i18n* 属性的节点；
 * 其余文案在渲染时通过 t('key') 取值。
 */

export type Lang = 'zh' | 'en';

/** 静态 DOM 文案：键即为 index.html 中 data-i18n 的值。 */
const staticText = {
  toolbarSessions: '本地会话',
  toolbarTools: '工具',
  toolbarSessionsLib: 'SSH会话',
  toolbarView: '视图',
  toolbarSplit: '分屏',
  toolbarMultiExec: '同步',
  toolbarBatch: '批量',
  toolbarTunneling: '隧道',
  toolbarSettings: '设置',
  toolbarLang: '语言',
  toolbarTheme: '主题',
  toolbarHelp: '帮助',
  toolbarXServer: 'X 服务',
  toolbarExit: '退出',

  quickConnectConnect: '连接',
  authPassword: '密码',
  authKey: '密钥',

  sidebarSessions: '会话',
  sidebarTools: '工具',
  sidebarMacros: '宏',

  welcomeLocal: '启动本地终端',
  welcomeRecover: '恢复上次会话',

  sftpUp: '上级目录',
  sftpRefresh: '刷新',
  sftpUpload: '上传文件或目录',
  sftpMkdir: '新建目录',
  sftpShowHidden: '显示隐藏文件',
  sftpHiddenCount: ({ n }: { n: number }) => `已隐藏 ${n} 项`,
  sftpCancel: '取消当前传输',

  settingsSave: '保存',
  commonCancel: '取消',
  commonClose: '关闭',
  commonConfirm: '确定',
  commonYes: '是', 
  commonNo: '否', 
};

/** 一条文案：定值字符串，或接受命名参数的函数。 */
export type Entry = string | ((params: any) => string);

/** 词条表。 */
export interface Dict {
  [key: string]: Entry | undefined;
}

const zh: Dict & typeof staticText = {
  ...staticText,

  // ---------- 工具栏提示 ----------
  tipSessions: '启动本地会话',
  tipSessionsLib: '新建SHH会话',
  tipTools: '工具',
  tipView: '视图',
  tipSplit: '分屏',
  tipMultiExec: '多标签同步输入',
  tipBatch: '批量执行命令',
  tipTunneling: '端口转发',
  tipSettings: '设置',
  tipLang: '切换语言',
  tipTheme: '切换主题',
  tipHelp: '帮助',
  tipXServer: 'X 服务',
  tipExit: '退出',

  // ---------- 快速连接 ----------
  quickPlaceholder: '快速连接…  例如 user@host:22',
  jumpPlaceholder: '跳板机（可空）  user@host:22',
  authMethodTitle: '认证方式',

  // ---------- 欢迎页 ----------
  welcomeLogo: 'RustTerm',
  welcomeSearchPlaceholder: '搜索已保存的会话或服务器名称…',
  welcomeRecent: '最近会话',
  welcomeNoSessionHint: '还没有保存的会话，先用上方快速连接。',
  welcomeSearchNoMatch: '没有匹配的会话',

  // ---------- 侧栏 ----------
  treeNoSessions: '（暂无保存的会话）',
  treeSSH: 'SSH 客户端',
  treeSFTP: 'SFTP 客户端',
  treeTelnet: 'Telnet 客户端',
  treeRDP: 'RDP 客户端',
  treeVNC: 'VNC 客户端',
  treeSpice: 'Spice 客户端',
  treeNoMacros: '暂无宏',
  sessionsCount: ({ n }: { n: number }) => `共 ${n} 个会话`,
  sessionSelected: ({ name }: { name: string }) => `已填入 ${name}`,

  // ---------- 状态栏 ----------
  statusReady: '就绪',
  statusPos: ({ row, col }: { row: number; col: number }) => `行 ${row} 列 ${col}`,
  statusNoTab: '没有打开的标签',
  statusNoActiveTab: '无活动标签',
  statusNeedSsh: '请先连接 SSH',
  statusLangSwitched: ({ lang }: { lang: string }) => `语言已切换为${lang}`,
  langNameZh: '中文',
  langNameEn: 'English',
  statusThemeLight: '已切换到浅色主题',
  statusThemeDark: '已切换到深色主题',
  statusFullscreenOn: '已进入全屏',
  statusFullscreenOff: '已退出全屏',
  statusFullscreenFailed: ({ err }: { err: string }) => `全屏切换失败: ${err}`,
  statusLayoutReset: '布局已重置',
  statusCleared: '已清屏',
  statusSelectedAll: '已全选',
  statusPasted: '已粘贴',
  statusPasteFailed: ({ err }: { err: string }) => `粘贴失败: ${err}`,
  statusCopyMenuCopied: '已复制当前选择',
  statusCopyFailed: ({ err }: { err: string }) => `复制失败: ${err}`,
  statusZoom: ({ size }: { size: number }) => `字号 ${size}`,
  statusDialogBusy: '请先关闭当前对话框',

  // ---------- 标签页 ----------
  tabRename: '重命名',
  tabRenamePrompt: '新标题：',
  tabMenuTitle: '标签操作',
  tabMenuBody: ({ title }: { title: string }) => `对「${title}」操作：\n1. 重命名\n2. 关闭\n3. 关闭其他\n4. 设置颜色\n输入编号：`,
  tabColor: '标签颜色',
  tabColorPrompt: '颜色（如 #ff0000，留空清除）：',
  tabClosed: ({ title }: { title: string }) => `会话 ${title} 已断开`,
  tabSplitClosed: '分屏会话已断开',
  tabGroupJoined: ({ title, group }: { title: string; group: number }) => `标签「${title}」加入同步组 G${group}`,
  tabGroupLeft: ({ title }: { title: string }) => `标签「${title}」退出同步组`,
  multiExecOn: ({ group }: { group: number }) => `多窗口同步已开启：按键将同步到 G${group} 全部窗口`,
  multiExecOff: '多窗口同步已关闭',
  termMultiExecRow: '同步输入（所有窗口一起执行命令）',

  // ---------- 分屏 ----------
  splitTitle: '分屏',
  splitPrompt: 'user@host:port，留空 = 本地终端：',
  splitNeedUser: '请使用 user@host 格式',
  splitOpened: ({ target }: { target: string }) => `分屏已打开：${target}`,
  splitLocal: '本地终端',
  splitClosed: '已关闭分屏',
  splitFailed: ({ err }: { err: string }) => `分屏失败: ${err}`,

  // ---------- 终端右键 ----------
  termMenuTitle: '终端操作',
  termMenuBody: ({ hasSelection }: { hasSelection: boolean }) =>
    hasSelection
      ? '1. 复制\n2. 粘贴\n3. 清屏\n4. 全选\n输入编号：'
      : '1. 粘贴\n2. 清屏\n3. 全选\n输入编号：',

  // ---------- 查找 ----------
  findPlaceholder: '查找…',
  findPrev: '上一个',
  findNext: '下一个',
  findNoMatch: '无匹配',

  // ---------- SFTP ----------
  sftpTitle: 'SFTP',
  sftpPathPlaceholder: '远程路径，如 /var/log',
  sftpNoSession: '请先连接 SSH 会话',
  sftpEmptyDir: '（空目录）',
  sftpAllHidden: '（目录内只有隐藏文件，已按设置隐藏）',
  sftpListFailed: ({ err }: { err: string }) => `列目录失败: ${err}`,
  sftpCount: ({ n }: { n: number }) => `${n} 项`,
  sftpMenuTitle: 'SFTP 操作',
  sftpMenuBody: ({ name }: { name: string }) => `对「${name}」：\n1. 下载\n2. 续传下载\n3. 重命名\n4. 删除\n输入编号：`,
  sftpRenameTitle: '重命名',
  sftpRenamePrompt: '新名称：',
  sftpRenamed: ({ from, to }: { from: string; to: string }) => `已重命名 ${from} → ${to}`,
  sftpRenameFailed: ({ err }: { err: string }) => `重命名失败: ${err}`,
  sftpDeleteTitle: '删除',
  sftpDeleteConfirm: ({ name }: { name: string }) => `删除 ${name}？`,
  sftpDeleteFailed: ({ err }: { err: string }) => `删除失败: ${err}`,
  sftpMkdirTitle: '新建目录',
  sftpMkdirPrompt: '目录名：',
  sftpMkdirDone: ({ name }: { name: string }) => `已创建目录 ${name}`,
  sftpMkdirFailed: ({ err }: { err: string }) => `创建失败: ${err}`,
  sftpUploadKindTitle: '上传类型',
  sftpUploadKindBody: '上传目录？（否 = 上传文件）',
  sftpUploading: ({ name }: { name: string }) => `上传 ${name}…`,
  sftpUploaded: ({ name }: { name: string }) => `已上传 ${name}`,
  sftpUploadFailed: ({ err }: { err: string }) => `上传失败: ${err}`,
  sftpUploadDirStarted: ({ name }: { name: string }) => `正在上传目录 ${name}…`,
  sftpDownloading: ({ name }: { name: string }) => `下载 ${name}`,
  sftpDownloadResumeStarted: '续传下载中…',
  sftpResumeFailed: ({ err }: { err: string }) => `续传失败: ${err}`,
  sftpTransferDone: ({ label }: { label: string }) => `${label} 完成`,
  sftpTransferFailed: ({ label, err }: { label: string; err: string }) => `${label} 失败: ${err}`,
  sftpCancelRequested: '已请求取消传输',

  // ---------- 传输队列 ----------
  queueCount: ({ n }: { n: number }) => `队列 ${n} 项`,

  // ---------- 批量命令 ----------
  batchTitle: '批量命令',
  batchBody: '勾选要执行的标签，输入命令。',
  batchPlaceholder: '要执行的命令，如 ls -la',
  batchRun: '执行',
  batchNeedTab: '请至少勾选一个标签',
  batchSent: ({ n }: { n: number }) => `已向 ${n} 个标签发送命令`,

  // ---------- Ping ----------
  pingTitle: 'Ping',
  pingPrompt: '主机或 IP：',
  pingStarted: ({ host }: { host: string }) => `已在本地终端启动 ping ${host}`,
  pingFailed: ({ err }: { err: string }) => `ping 失败: ${err}`,

  // ---------- Telnet ----------
  telnetTitle: 'Telnet',
  telnetHostPrompt: '主机：',
  telnetPortPrompt: '端口：',
  telnetConnected: ({ host, port }: { host: string; port: string }) => `Telnet 已连接 ${host}:${port}`,
  telnetFailed: ({ err }: { err: string }) => `Telnet 失败: ${err}`,

  // ---------- 端口转发 ----------
  tunnelTitle: '端口转发',
  tunnelEmpty: '（暂无隧道）',
  tunnelAdd: '新增',
  tunnelClose: '关闭',
  tunnelDelete: '删除',
  tunnelSpecTitle: '端口转发',
  tunnelSpecPrompt: 'local_port:remote_host:remote_port\n例如 8080:localhost:80',
  tunnelFormatError: '格式错误，应为 local_port:remote_host:remote_port',
  tunnelCreated: ({ port, host, rport }: { port: number; host: string; rport: number }) =>
    `隧道已建立：127.0.0.1:${port} → ${host}:${rport}`,
  tunnelFailed: ({ err }: { err: string }) => `隧道失败: ${err}`,

  // ---------- SSH 连接 ----------
  sshFormatError: '请使用 user@host:port 格式',
  sshPasswordTitle: 'SSH 密码',
  sshPasswordPrompt: ({ user, host }: { user: string; host: string }) => `请输入 ${user}@${host} 的密码：`,
  sshKeyTitle: '密钥认证',
  sshKeyPathPrompt: '私钥路径：',
  sshKeyPassphrasePrompt: '私钥口令（无则留空）：',
  sshJumpPasswordTitle: '跳板机密码',
  sshJumpPasswordPrompt: ({ user, host }: { user: string; host: string }) => `请输入 ${user}@${host} 的密码：`,
  sshConnectingKey: ({ user, host }: { user: string; host: string }) => `正在连接 ${user}@${host}（密钥）…`,
  sshConnectingJump: ({ host, target }: { host: string; target: string }) => `经跳板机 ${host} 连接 ${target}…`,
  sshConnecting: ({ user, host }: { user: string; host: string }) => `正在连接 ${user}@${host}…`,
  sshTimeout: '连接超时（15 秒）',
  sshConnected: ({ user, host }: { user: string; host: string }) => `已连接 ${user}@${host}`,
  sshFailed: ({ err }: { err: string }) => `连接失败: ${err}`,
  sshRetryTitle: '重试',
  sshRetryBody: '连接失败，是否重试？',
  sshLocalTitle: ({ n }: { n: number }) => `本地:${n}`,
  sshTabTitle: ({ user, host }: { user: string; host: string }) => `SSH:${user}@${host}`,
  telnetTabTitle: ({ host }: { host: string }) => `Telnet:${host}`,

  // ---------- 会话管理 ----------
  sessDeleteTitle: '删除会话',
  sessDeleteConfirm: ({ name }: { name: string }) => `删除 ${name}？`,
  sessReadFailed: ({ err }: { err: string }) => `读取会话失败: ${err}`,
  sessNoHistory: '无历史会话',
  sessRecovered: ({ name }: { name: string }) => `已填入 ${name}`,
  sessGroup: '默认分组',
  sessSaveTitle: '保存会话',
  sessGroupPrompt: '分组名（可空）：',
  sessColorPrompt: '颜色（如 #ff0000，可空）：',
  sessSaved: '会话已保存',
  sessRememberPasswordAsk: '是否把密码保存到系统凭据库？（Windows 凭据管理器 / macOS 钥匙串 / Linux Secret Service）',
  sessPasswordSaved: '密码已保存到系统凭据库',
  sessPasswordCleared: '已清除该会话保存的密码',
  sessClearPassword: '清除已保存的密码',
  sessPasswordMark: '已存密码',
  secretStoreUnavailable: ({ detail }: { detail: string }) => `系统凭据库不可用，密码不会被保存：${detail}`,
  secretPasswordRequired: '该会话已勾选记住密码，请输入密码或取消勾选',
  sessSaveFailed: ({ err }: { err: string }) => `保存失败: ${err}`,
  sessNeedQuickInput: '请先在快速连接框填入 user@host',
  sessImportedPutty: '已从 PuTTY 导入会话',
  sessImportPuttyFailed: ({ err }: { err: string }) => `导入失败: ${err}`,
  sessExported: '会话已导出',
  sessExportFailed: ({ err }: { err: string }) => `导出失败: ${err}`,
  sessImported: ({ n }: { n: number }) => `已导入 ${n} 个会话`,
  sessImportFailed: ({ err }: { err: string }) => `导入失败: ${err}`,

  scanTitle: '端口扫描',
  scanTargets: '目标（IP 范围或 CIDR）：',
  scanPorts: '端口（逗号分隔，支持范围）：',
  scanConcurrency: '并发数：',
  scanTimeout: '超时（毫秒）：',
  scanRun: '开始',
  scanStop: '停止',
  scanProgress: ({ scanned, total, open }: { scanned: number; total: number; open: number }) =>
    `${scanned} / ${total}  已发现 ${open} 个开放端口`,
  scanDone: ({ open }: { open: number }) => `扫描完成，共 ${open} 个开放端口`,
  scanStopped: '扫描已停止',
  scanOpenPort: '开放',
  scanNoResult: '（暂无开放端口）',
  scanInvalidTarget: '目标格式错误',
  scanInvalidPorts: '端口格式错误',

  toolbarAi: 'AI',
  tipAi: 'AI 助手',
  aiTitle: 'AI 助手',
  aiPlaceholder: '问点什么…（Enter 发送，Shift+Enter 换行）',
  aiSend: '发送',
  aiClear: '清空对话',
  aiSettings: 'AI 配置',
  aiConfigTitle: 'AI 配置',
  aiProvider: '提供方',
  aiApiKey: 'API Key',
  aiBaseUrl: 'Base URL',
  aiModel: '模型',
  aiConfigHint: 'API Key 仅保存在本地。Ollama 不需要 Key。',
  aiDangerConfirm: ({ cmd }: { cmd: string }) => `⚠️ 即将执行危险命令：\n\n${cmd}\n\n确认执行？`,
  aiExecuted: '已执行',
  aiError: ({ err }: { err: string }) => `AI 调用失败：${err}`,
  aiReadOnly: '只读模式（禁用写命令）',
  aiMaxHistory: '对话历史最多保留（条）',
  aiContextLines: '终端上下文最近行数',
  aiContextMaxLineLen: '上下文单行最大字符数',
  aiReadOnlyBlocked: '只读模式：写命令已被禁用',

  aiAgentMode: 'Agent',
  aiAgentTip: 'Agent 模式：AI 自动多步执行任务',
  aiAgentRunning: ({ step, max }: { step: number; max: number }) => `Agent 运行中… 第 ${step}/${max} 步`,
  aiAgentDone: 'Agent 任务完成',
  aiAgentStopped: 'Agent 已停止',
  aiAgentMaxSteps: ({ max }: { max: number }) => `Agent 达到最大步数 ${max}，已停止`,
  aiAgentNoCommands: 'Agent 没有生成可执行的命令，已停止',
  aiAgentApprove: ({ cmd }: { cmd: string }) => `⚠️ Agent 想执行危险命令：\n\n${cmd}\n\n允许？`,
  aiStop: '停止 Agent',
    
  // RDP（新增）
  rdpHostPrompt: 'RDP 服务器地址：',
  rdpPortPrompt: '端口（默认 3389）：',
  rdpUserPrompt: '用户名：',
  rdpPasswordPrompt: '密码：',
  rdpConnecting: ({ host, port }: { host: string; port: number }) => `正在连接 RDP ${host}:${port}…`,
  rdpConnected: ({ host }: { host: string }) => `RDP 已连接 ${host}`,
  rdpTabTitle: ({ host }: { host: string }) => `RDP: ${host}`,
  rdpStateConnecting: 'RDP：正在连接…',
  rdpStateAuthenticating: 'RDP：正在认证…',
  rdpStateActive: 'RDP：已连接',
  rdpStateReconnecting: 'RDP：重连中…',
  rdpStateDisconnected: 'RDP：已断开',
  rdpStateFailed: 'RDP：连接失败',
  rdpErrorTransport: 'RDP：网络中断',
  rdpErrorAuthentication: 'RDP：认证失败',
  rdpErrorSession: 'RDP：会话已结束',
  rdpErrorInternal: 'RDP：内部错误',

  // VNC（新增）
  vncHostPrompt: 'VNC 服务器地址：',
  vncPortPrompt: '端口（默认 5900）：',
  vncPasswordPrompt: 'VNC 密码：',
  vncConnecting: ({ host, port }: { host: string; port: number }) => `正在连接 VNC ${host}:${port}…`,
  vncConnected: ({ host }: { host: string }) => `VNC 已连接 ${host}`,
  vncTabTitle: ({ host }: { host: string }) => `VNC: ${host}`,

  sessNeedRemoteInfo: '无法获取远程桌面会话信息',

  // SPICE（新增）
  spiceTitle: 'SPICE',
  spiceHostPrompt: 'SPICE 服务器地址：',
  spicePortPrompt: '端口（默认 5930）：',
  spicePasswordPrompt: 'SPICE 密码（ticket）：',
  spiceConnecting: ({ host, port }: { host: string; port: number }) => `正在连接 SPICE ${host}:${port}…`,
  spiceConnected: ({ host }: { host: string }) => `SPICE 已连接 ${host}`,
  spiceTabTitle: ({ host }: { host: string }) => `SPICE: ${host}`,
  spiceFailed: ({ err }: { err: string }) => `SPICE 连接失败: ${err}`,

  // ---------- 设置 ----------
  settingsTitle: '设置',
  settingsFontSize: '字体大小：',
  settingsCursorBlink: '光标闪烁：',
  settingsScrollback: '回滚缓冲行数：',
  settingsHostKey: '主机密钥校验：',
  hostKeyStrict: '严格（拒绝未知主机）',
  hostKeyAcceptNew: '记录新主机，拒绝变更（推荐）',
  hostKeyInsecure: '不校验（兼容老设备）',
  hostKeyHintAcceptNew: '首次连接会把主机公钥写入 known_hosts，之后密钥变更将被拒绝。',
  hostKeyHintStrict: '只允许 known_hosts 中已有的主机，新主机一律拒绝。',
  hostKeyHintInsecure: '警告：不校验主机身份，无法防御中间人攻击。',
  hostKeyHintPath: ({ path }: { path: string }) => `记录文件：${path}`,
  hostKeyHintNoPath: '记录文件：无法定位用户主目录',
  settingsSaved: '设置已保存',

  // ---------- 关于 ----------
  aboutTitle: '关于 RustTerm',
  aboutDesc: 'SSH & SFTP 终端工具',
  aboutBuilt: '使用 Tauri + Rust + xterm.js 构建',
  aboutShortcuts: '快捷键：Ctrl+T 本地终端 · Ctrl+N 新建 SSH · Ctrl+W 关闭标签 · Ctrl+F 查找 · Ctrl+B 侧栏 · Ctrl+L 会话库 · F11 全屏',
  aboutLicense: 'MIT License',

  // ---------- 宏 ----------
  macroTitle: '回放宏',
  macroRecording: '● 开始录制宏…再点一次结束',
  macroSaveTitle: '保存宏',
  macroNamePrompt: '宏名称：',
  macroSaved: ({ name }: { name: string }) => `宏「${name}」已保存`,
  macroNone: '没有已保存的宏',
  macroReplayed: ({ name }: { name: string }) => `已回放宏「${name}」`,
  macroBytes: ({ n }: { n: number }) => `${n} 字节`,

  // ---------- X server ----------
  x11Title: 'X11',
  x11SetDisplay: 'X server 已启动。\n是否为当前 SSH 会话自动设置 DISPLAY？',
  x11DisplaySet: '已设置 DISPLAY',
  x11SetFailed: ({ err }: { err: string }) => `设置 DISPLAY 失败: ${err}`,
  x11StartFailed: ({ err }: { err: string }) => `启动 X server 失败: ${err}`,

  xserverConfigTitle: 'X server 配置',
  xserverConfigHint: '按顺序尝试。第一个存在的可执行文件会被使用。',
  xserverAdd: '添加',
  xserverProgram: '可执行文件路径',
  xserverArgs: '参数（空格分隔）',
  xserverRemove: '删除',
  xserverSaved: 'X server 配置已保存',

  // ---------- 工具 / 视图菜单 ----------
  toolsMenuTitle: '工具',
  toolsMenuBody: '1. SSH\n2. SFTP\n3. Telnet\n4. 端口转发\n5. Ping\n6. 包检查\n编号：',
  viewMenuTitle: '视图',
  viewMenuBody: '1. 侧栏\n2. SFTP\n3. 放大\n4. 缩小\n5. 重置字体\n6. 全屏\n编号：',
  packagesTitle: '已安装组件',

  // ---------- 外链 / 其他 ----------
  openLinkFailed: ({ err }: { err: string }) => `打开链接失败: ${err}`,
  rdpTitle: 'RDP',
  rdpFailed: ({ err }: { err: string }) => `启动失败: ${err}`,
  vncTitle: 'VNC',
  vncFailed: ({ err }: { err: string }) => `打开失败: ${err}`,
  sessionRestoreHint: ({ n }: { n: number }) => `上次有 ${n} 个 SSH 标签，请在会话库中重连`,

  // ---------- 后端错误码 ----------
  errSessionNotFound: '会话不存在或已断开',
  errAuthFailed: '认证失败，请检查用户名或密码',
  errKeyAuthFailed: '密钥认证失败，请检查私钥或口令',
  errJumpAuthFailed: '跳板机认证失败',
  errTargetAuthFailed: '目标主机认证失败',
  errCancelled: '传输已取消',
  errConnect: ({ err }: { err: string }) => `无法连接到主机: ${err}`,
  errHostKeyChanged: ({ line }: { line: string }) => `主机密钥已变更（known_hosts 第 ${line} 行）！这可能意味着中间人攻击，已中断连接。请核实后手动更新该记录。`,
  errHostKeyUnknown: '主机密钥未记录，当前为「严格」策略，已拒绝连接。请先在 known_hosts 中登记该主机。',
  errHostKeyNoHome: '无法定位用户主目录，取不到 known_hosts，已按安全策略拒绝连接。',
  errHostKeyIo: ({ err }: { err: string }) => `读取 known_hosts 失败: ${err}`,
  errXserverNotFound: '未找到本地 X server（VcXsrv / Xming），请先安装',
  errXserverStartFailed: ({ err }: { err: string }) => `启动 X server 失败: ${err}`,
  errXserverReadyTimeout: 'X server 启动超时，请检查是否有弹窗被遮挡或端口被占用',
  errXserverNotRunning: 'X server 未运行',
  errXserverNoFreeDisplay: '没有空闲的 X display 可用',
  statusXserverStarted: ({ display }: { display: string }) => `X server 已启动（display ${display}）`,
  statusXserverStopped: 'X server 已停止',
  errUnknown: ({ err }: { err: string }) => `操作失败: ${err}`,
};

const en: Dict & typeof staticText = {
  ...staticText,

  toolbarSessions: 'Local Session',
  toolbarTools: 'Tools',
  toolbarSessionsLib: 'SSH Session',
  toolbarView: 'View',
  toolbarSplit: 'Split',
  toolbarMultiExec: 'Sync',
  toolbarBatch: 'Batch',
  toolbarTunneling: 'Tunnel',
  toolbarSettings: 'Settings',
  toolbarLang: 'Language',
  toolbarTheme: 'Theme',
  toolbarHelp: 'Help',
  toolbarXServer: 'X server',
  toolbarExit: 'Exit',

  quickConnectConnect: 'Connect',
  authPassword: 'Password',
  authKey: 'Key',

  sidebarSessions: 'Sessions',
  sidebarTools: 'Tools',
  sidebarMacros: 'Macros',

  welcomeLocal: 'Start local terminal',
  welcomeRecover: 'Recover previous sessions',

  sftpUp: 'Parent directory',
  sftpRefresh: 'Refresh',
  sftpUpload: 'Upload file or folder',
  sftpMkdir: 'New folder',
  sftpShowHidden: 'Show hidden files',
  sftpHiddenCount: ({ n }: { n: number }) => `${n} hidden`,
  sftpCancel: 'Cancel current transfer',

  settingsSave: 'Save',
  commonCancel: 'Cancel',
  commonClose: 'Close',
  commonConfirm: 'OK',
  commonYes: 'Yes',
  commonNo: 'No',

  // ---------- toolbar tips ----------
  tipSessions: 'Start Local Session',
  tipSessionsLib: 'New SSH Session',
  tipTools: 'Tools',
  tipView: 'View',
  tipSplit: 'Split',
  tipMultiExec: 'Synchronized input across tabs',
  tipBatch: 'Run a command on many tabs',
  tipTunneling: 'Port forwarding',
  tipSettings: 'Settings',
  tipLang: 'Switch language',
  tipTheme: 'Switch theme',
  tipHelp: 'Help',
  tipXServer: 'X server',
  tipExit: 'Exit',

  // ---------- quick connect ----------
  quickPlaceholder: 'Quick connect…  e.g. user@host:22',
  jumpPlaceholder: 'Jump host (optional)  user@host:22',
  authMethodTitle: 'Authentication',

  // ---------- welcome ----------
  welcomeLogo: 'RustTerm',
  welcomeSearchPlaceholder: 'Find a saved session or server name…',
  welcomeRecent: 'Recent sessions',
  welcomeNoSessionHint: 'Nothing saved yet — use quick connect above.',
  welcomeSearchNoMatch: 'No matching session',

  // ---------- sidebar ----------
  treeNoSessions: '(no saved sessions)',
  treeSSH: 'SSH client',
  treeSFTP: 'SFTP client',
  treeTelnet: 'Telnet client',
  treeRDP: 'RDP client',
  treeVNC: 'VNC client',
  treeSpice: 'Spice client',
  treeNoMacros: 'No macros',
  sessionsCount: ({ n }: { n: number }) => `${n} session${n === 1 ? '' : 's'}`,
  sessionSelected: ({ name }: { name: string }) => `Selected ${name}`,

  // ---------- status bar ----------
  statusReady: 'Ready',
  statusPos: ({ row, col }: { row: number; col: number }) => `Ln ${row}, Col ${col}`,
  statusNoTab: 'No open tabs',
  statusNoActiveTab: 'No active tab',
  statusNeedSsh: 'Connect an SSH session first',
  statusLangSwitched: ({ lang }: { lang: string }) => `Language switched to ${lang}`,
  langNameZh: '中文',
  langNameEn: 'English',
  statusThemeLight: 'Light theme',
  statusThemeDark: 'Dark theme',
  statusFullscreenOn: 'Entered fullscreen',
  statusFullscreenOff: 'Left fullscreen',
  statusFullscreenFailed: ({ err }: { err: string }) => `Fullscreen toggle failed: ${err}`,
  statusLayoutReset: 'Layout reset',
  statusCleared: 'Cleared',
  statusSelectedAll: 'Selected all',
  statusPasted: 'Pasted',
  statusPasteFailed: ({ err }: { err: string }) => `Paste failed: ${err}`,
  statusCopyMenuCopied: 'Copied selection',
  statusCopyFailed: ({ err }: { err: string }) => `Copy failed: ${err}`,
  statusZoom: ({ size }: { size: number }) => `Font size ${size}`,
  statusDialogBusy: 'Close the current dialog first',

  // ---------- tabs ----------
  tabRename: 'Rename',
  tabRenamePrompt: 'New title:',
  tabMenuTitle: 'Tab actions',
  tabMenuBody: ({ title }: { title: string }) =>
    `Actions for “${title}”:\n1. Rename\n2. Close\n3. Close others\n4. Set colour\nEnter a number:`,
  tabColor: 'Tab colour',
  tabColorPrompt: 'Colour (e.g. #ff0000, empty to clear):',
  tabClosed: ({ title }: { title: string }) => `Session ${title} disconnected`,
  tabSplitClosed: 'Split session disconnected',
  tabGroupJoined: ({ title, group }: { title: string; group: number }) => `Tab “${title}” joined sync group G${group}`,
  tabGroupLeft: ({ title }: { title: string }) => `Tab “${title}” left the sync group`,
  multiExecOn: ({ group }: { group: number }) => `Multi-window sync ON: keystrokes are sent to every window in G${group}`,
  multiExecOff: 'Multi-window sync OFF',
  termMultiExecRow: 'Synchronized input (all windows run commands together)',

  // ---------- split ----------
  splitTitle: 'Split',
  splitPrompt: 'user@host:port, empty = local terminal:',
  splitNeedUser: 'Use the user@host format',
  splitOpened: ({ target }: { target: string }) => `Split opened: ${target}`,
  splitLocal: 'local terminal',
  splitClosed: 'Split closed',
  splitFailed: ({ err }: { err: string }) => `Split failed: ${err}`,

  // ---------- terminal context menu ----------
  termMenuTitle: 'Terminal',
  termMenuBody: ({ hasSelection }: { hasSelection: boolean }) =>
    hasSelection
      ? '1. Copy\n2. Paste\n3. Clear\n4. Select all\nEnter a number:'
      : '1. Paste\n2. Clear\n3. Select all\nEnter a number:',

  // ---------- find ----------
  findPlaceholder: 'Find…',
  findPrev: 'Previous',
  findNext: 'Next',
  findNoMatch: 'No match',

  // ---------- SFTP ----------
  sftpTitle: 'SFTP',
  sftpPathPlaceholder: 'Remote path, e.g. /var/log',
  sftpNoSession: 'Connect an SSH session first',
  sftpEmptyDir: '(empty directory)',
  sftpAllHidden: '(only hidden files here — currently hidden by your setting)',
  sftpListFailed: ({ err }: { err: string }) => `Listing failed: ${err}`,
  sftpCount: ({ n }: { n: number }) => `${n} item${n === 1 ? '' : 's'}`,
  sftpMenuTitle: 'SFTP actions',
  sftpMenuBody: ({ name }: { name: string }) =>
    `Actions for “${name}”:\n1. Download\n2. Resume download\n3. Rename\n4. Delete\nEnter a number:`,
  sftpRenameTitle: 'Rename',
  sftpRenamePrompt: 'New name:',
  sftpRenamed: ({ from, to }: { from: string; to: string }) => `Renamed ${from} → ${to}`,
  sftpRenameFailed: ({ err }: { err: string }) => `Rename failed: ${err}`,
  sftpDeleteTitle: 'Delete',
  sftpDeleteConfirm: ({ name }: { name: string }) => `Delete ${name}?`,
  sftpDeleteFailed: ({ err }: { err: string }) => `Delete failed: ${err}`,
  sftpMkdirTitle: 'New folder',
  sftpMkdirPrompt: 'Folder name:',
  sftpMkdirDone: ({ name }: { name: string }) => `Created folder ${name}`,
  sftpMkdirFailed: ({ err }: { err: string }) => `Create failed: ${err}`,
  sftpUploadKindTitle: 'Upload type',
  sftpUploadKindBody: 'Upload a folder? (No = upload a file)',
  sftpUploading: ({ name }: { name: string }) => `Uploading ${name}…`,
  sftpUploaded: ({ name }: { name: string }) => `Uploaded ${name}`,
  sftpUploadFailed: ({ err }: { err: string }) => `Upload failed: ${err}`,
  sftpUploadDirStarted: ({ name }: { name: string }) => `Uploading folder ${name}…`,
  sftpDownloading: ({ name }: { name: string }) => `Download ${name}`,
  sftpDownloadResumeStarted: 'Resuming download…',
  sftpResumeFailed: ({ err }: { err: string }) => `Resume failed: ${err}`,
  sftpTransferDone: ({ label }: { label: string }) => `${label} finished`,
  sftpTransferFailed: ({ label, err }: { label: string; err: string }) => `${label} failed: ${err}`,
  sftpCancelRequested: 'Cancellation requested',

  // ---------- transfer queue ----------
  queueCount: ({ n }: { n: number }) => `${n} queued`,

  // ---------- batch ----------
  batchTitle: 'Batch command',
  batchBody: 'Tick the tabs to run on, then type a command.',
  batchPlaceholder: 'Command to run, e.g. ls -la',
  batchRun: 'Run',
  batchNeedTab: 'Tick at least one tab',
  batchSent: ({ n }: { n: number }) => `Sent to ${n} tab${n === 1 ? '' : 's'}`,

  // ---------- ping ----------
  pingTitle: 'Ping',
  pingPrompt: 'Host or IP:',
  pingStarted: ({ host }: { host: string }) => `ping ${host} started in a local terminal`,
  pingFailed: ({ err }: { err: string }) => `ping failed: ${err}`,

  // ---------- telnet ----------
  telnetTitle: 'Telnet',
  telnetHostPrompt: 'Host:',
  telnetPortPrompt: 'Port:',
  telnetConnected: ({ host, port }: { host: string; port: string }) => `Telnet connected to ${host}:${port}`,
  telnetFailed: ({ err }: { err: string }) => `Telnet failed: ${err}`,

  // ---------- tunnelling ----------
  tunnelTitle: 'Port forwarding',
  tunnelEmpty: '(no tunnels)',
  tunnelAdd: 'Add',
  tunnelClose: 'Close',
  tunnelDelete: 'Delete',
  tunnelSpecTitle: 'Port forwarding',
  tunnelSpecPrompt: 'local_port:remote_host:remote_port\ne.g. 8080:localhost:80',
  tunnelFormatError: 'Bad format — expected local_port:remote_host:remote_port',
  tunnelCreated: ({ port, host, rport }: { port: number; host: string; rport: number }) =>
    `Tunnel up: 127.0.0.1:${port} → ${host}:${rport}`,
  tunnelFailed: ({ err }: { err: string }) => `Tunnel failed: ${err}`,

  // ---------- SSH ----------
  sshFormatError: 'Use the user@host:port format',
  sshPasswordTitle: 'SSH password',
  sshPasswordPrompt: ({ user, host }: { user: string; host: string }) => `Password for ${user}@${host}:`,
  sshKeyTitle: 'Key authentication',
  sshKeyPathPrompt: 'Private key path:',
  sshKeyPassphrasePrompt: 'Key passphrase (empty if none):',
  sshJumpPasswordTitle: 'Jump host password',
  sshJumpPasswordPrompt: ({ user, host }: { user: string; host: string }) => `Password for ${user}@${host}:`,
  sshConnectingKey: ({ user, host }: { user: string; host: string }) => `Connecting to ${user}@${host} (key)…`,
  sshConnectingJump: ({ host, target }: { host: string; target: string }) => `Connecting to ${target} via ${host}…`,
  sshConnecting: ({ user, host }: { user: string; host: string }) => `Connecting to ${user}@${host}…`,
  sshTimeout: 'Connection timed out (15 s)',
  sshConnected: ({ user, host }: { user: string; host: string }) => `Connected to ${user}@${host}`,
  sshFailed: ({ err }: { err: string }) => `Connection failed: ${err}`,
  sshRetryTitle: 'Retry',
  sshRetryBody: 'Connection failed. Retry?',
  sshLocalTitle: ({ n }: { n: number }) => `Local:${n}`,
  sshTabTitle: ({ user, host }: { user: string; host: string }) => `SSH:${user}@${host}`,
  telnetTabTitle: ({ host }: { host: string }) => `Telnet:${host}`,

  // ---------- session manager ----------
  sessDeleteTitle: 'Delete session',
  sessDeleteConfirm: ({ name }: { name: string }) => `Delete ${name}?`,
  sessReadFailed: ({ err }: { err: string }) => `Could not read sessions: ${err}`,
  sessNoHistory: 'No previous sessions',
  sessRecovered: ({ name }: { name: string }) => `Filled in ${name}`,
  sessGroup: 'Default',
  sessSaveTitle: 'Save session',
  sessGroupPrompt: 'Group name (optional):',
  sessColorPrompt: 'Colour (e.g. #ff0000, optional):',
  sessSaved: 'Session saved',
  sessRememberPasswordAsk: 'Save the password in the system credential store? (Windows Credential Manager / macOS Keychain / Linux Secret Service)',
  sessPasswordSaved: 'Password saved to the system credential store',
  sessPasswordCleared: 'Saved password for this session was removed',
  sessClearPassword: 'Forget saved password',
  sessPasswordMark: 'password saved',
  secretStoreUnavailable: ({ detail }: { detail: string }) => `System credential store unavailable, the password will not be saved: ${detail}`,
  secretPasswordRequired: 'This session is marked to remember the password — enter one or uncheck the option',
  sessSaveFailed: ({ err }: { err: string }) => `Save failed: ${err}`,
  sessNeedQuickInput: 'Type user@host in the quick connect box first',
  sessImportedPutty: 'Imported PuTTY sessions',
  sessImportPuttyFailed: ({ err }: { err: string }) => `Import failed: ${err}`,
  sessExported: 'Sessions exported',
  sessExportFailed: ({ err }: { err: string }) => `Export failed: ${err}`,
  sessImported: ({ n }: { n: number }) => `Imported ${n} session${n === 1 ? '' : 's'}`,
  sessImportFailed: ({ err }: { err: string }) => `Import failed: ${err}`,

  scanTitle: 'Port scan',
  scanTargets: 'Targets (IP range or CIDR):',
  scanPorts: 'Ports (comma separated, ranges allowed):',
  scanConcurrency: 'Concurrency:',
  scanTimeout: 'Timeout (ms):',
  scanRun: 'Start',
  scanStop: 'Stop',
  scanProgress: ({ scanned, total, open }: { scanned: number; total: number; open: number }) =>
    `${scanned} / ${total}  ${open} open`,
  scanDone: ({ open }: { open: number }) => `Scan complete: ${open} open ports`,
  scanStopped: 'Scan stopped',
  scanOpenPort: 'open',
  scanNoResult: '(no open ports yet)',
  scanInvalidTarget: 'Bad target format',
  scanInvalidPorts: 'Bad port format',

  toolbarAi: 'AI',
  tipAi: 'AI assistant',
  aiTitle: 'AI assistant',
  aiPlaceholder: 'Ask something… (Enter to send, Shift+Enter for newline)',
  aiSend: 'Send',
  aiClear: 'Clear chat',
  aiSettings: 'AI settings',
  aiConfigTitle: 'AI settings',
  aiProvider: 'Provider',
  aiApiKey: 'API Key',
  aiBaseUrl: 'Base URL',
  aiModel: 'Model',
  aiConfigHint: 'API Key is stored locally. Ollama does not need a key.',
  aiDangerConfirm: ({ cmd }: { cmd: string }) => `⚠️ About to run a dangerous command:\n\n${cmd}\n\nConfirm?`,
  aiExecuted: 'Executed',
  aiError: ({ err }: { err: string }) => `AI call failed: ${err}`,
  aiReadOnly: 'Read-only mode (disable write commands)',
  aiMaxHistory: 'Max chat history (messages)',
  aiContextLines: 'Recent terminal lines for context',
  aiContextMaxLineLen: 'Max chars per context line',
  aiReadOnlyBlocked: 'Read-only mode: write commands are disabled',

  aiAgentMode: 'Agent',
  aiAgentTip: 'Agent mode: AI runs multi-step tasks automatically',
  aiAgentRunning: ({ step, max }: { step: number; max: number }) => `Agent running… step ${step}/${max}`,
  aiAgentDone: 'Agent task completed',
  aiAgentStopped: 'Agent stopped',
  aiAgentMaxSteps: ({ max }: { max: number }) => `Agent reached max steps (${max}), stopped`,
  aiAgentNoCommands: 'Agent produced no executable commands, stopped',
  aiAgentApprove: ({ cmd }: { cmd: string }) => `⚠️ Agent wants to run a dangerous command:\n\n${cmd}\n\nAllow?`,
  aiStop: 'Stop Agent',

  rdpHostPrompt: 'RDP server address:',
  rdpPortPrompt: 'Port (default 3389):',
  rdpUserPrompt: 'Username:',
  rdpPasswordPrompt: 'Password:',
  rdpConnecting: ({ host, port }: { host: string; port: number }) => `Connecting to RDP ${host}:${port}…`,
  rdpConnected: ({ host }: { host: string }) => `RDP connected to ${host}`,
  rdpTabTitle: ({ host }: { host: string }) => `RDP: ${host}`,
  rdpStateConnecting: 'RDP: connecting…',
  rdpStateAuthenticating: 'RDP: authenticating…',
  rdpStateActive: 'RDP: connected',
  rdpStateReconnecting: 'RDP: reconnecting…',
  rdpStateDisconnected: 'RDP: disconnected',
  rdpStateFailed: 'RDP: connection failed',
  rdpErrorTransport: 'RDP: network interrupted',
  rdpErrorAuthentication: 'RDP: authentication failed',
  rdpErrorSession: 'RDP: session ended',
  rdpErrorInternal: 'RDP: internal error',

  vncHostPrompt: 'VNC server address:',
  vncPortPrompt: 'Port (default 5900):',
  vncPasswordPrompt: 'VNC password:',
  vncConnecting: ({ host, port }: { host: string; port: number }) => `Connecting to VNC ${host}:${port}…`,
  vncConnected: ({ host }: { host: string }) => `VNC connected to ${host}`,
  vncTabTitle: ({ host }: { host: string }) => `VNC: ${host}`,

  sessNeedRemoteInfo: 'Cannot get remote desktop session info',

  spiceTitle: 'SPICE',
  spiceHostPrompt: 'SPICE server address:',
  spicePortPrompt: 'Port (default 5930):',
  spicePasswordPrompt: 'SPICE password (ticket):',
  spiceConnecting: ({ host, port }: { host: string; port: number }) => `Connecting to SPICE ${host}:${port}…`,
  spiceConnected: ({ host }: { host: string }) => `SPICE connected to ${host}`,
  spiceTabTitle: ({ host }: { host: string }) => `SPICE: ${host}`,
  spiceFailed: ({ err }: { err: string }) => `SPICE connection failed: ${err}`,
  
  // ---------- settings ----------
  settingsTitle: 'Settings',
  settingsFontSize: 'Font size:',
  settingsCursorBlink: 'Cursor blink:',
  settingsScrollback: 'Scrollback lines:',
  settingsHostKey: 'Host key checking:',
  hostKeyStrict: 'Strict (reject unknown hosts)',
  hostKeyAcceptNew: 'Record new hosts, reject changes (recommended)',
  hostKeyInsecure: 'Off (for legacy devices)',
  hostKeyHintAcceptNew: 'The host key is recorded on first connect; later changes are rejected.',
  hostKeyHintStrict: 'Only hosts already present in known_hosts are allowed.',
  hostKeyHintInsecure: 'Warning: host identity is not verified; vulnerable to man-in-the-middle attacks.',
  hostKeyHintPath: ({ path }: { path: string }) => `Recorded in: ${path}`,
  hostKeyHintNoPath: 'Recorded in: cannot locate the user home directory',
  settingsSaved: 'Settings saved',

  // ---------- about ----------
  aboutTitle: 'About RustTerm',
  aboutDesc: 'SSH & SFTP terminal',
  aboutBuilt: 'Built with Tauri + Rust + xterm.js',
  aboutShortcuts: 'Shortcuts: Ctrl+T local terminal · Ctrl+N new SSH · Ctrl+W close tab · Ctrl+F find · Ctrl+B sidebar · Ctrl+L sessions · F11 fullscreen',
  aboutLicense: 'MIT License',

  // ---------- macros ----------
  macroTitle: 'Play macro',
  macroRecording: '● Recording macro… click again to stop',
  macroSaveTitle: 'Save macro',
  macroNamePrompt: 'Macro name:',
  macroSaved: ({ name }: { name: string }) => `Macro “${name}” saved`,
  macroNone: 'No saved macros',
  macroReplayed: ({ name }: { name: string }) => `Replayed macro “${name}”`,
  macroBytes: ({ n }: { n: number }) => `${n} bytes`,

  // ---------- X server ----------
  x11Title: 'X11',
  x11SetDisplay: 'X server started.\nSet DISPLAY automatically for the current SSH session?',
  x11DisplaySet: 'DISPLAY set',
  x11SetFailed: ({ err }: { err: string }) => `Could not set DISPLAY: ${err}`,
  x11StartFailed: ({ err }: { err: string }) => `Could not start X server: ${err}`,

  xserverConfigTitle: 'X server configuration',
  xserverConfigHint: 'Tried in order. The first existing executable is used.',
  xserverAdd: 'Add',
  xserverProgram: 'Executable path',
  xserverArgs: 'Arguments (space-separated)',
  xserverRemove: 'Remove',
  xserverSaved: 'X server configuration saved',

  // ---------- tools / view menus ----------
  toolsMenuTitle: 'Tools',
  toolsMenuBody: '1. SSH\n2. SFTP\n3. Telnet\n4. Port forwarding\n5. Ping\n6. Check packages\nEnter a number:',
  viewMenuTitle: 'View',
  viewMenuBody: '1. Sidebar\n2. SFTP\n3. Zoom in\n4. Zoom out\n5. Reset font\n6. Fullscreen\nEnter a number:',
  packagesTitle: 'Installed components',

  // ---------- external ----------
  openLinkFailed: ({ err }: { err: string }) => `Could not open link: ${err}`,
  rdpTitle: 'RDP',
  rdpFailed: ({ err }: { err: string }) => `Could not start: ${err}`,
  vncTitle: 'VNC',
  vncFailed: ({ err }: { err: string }) => `Could not open: ${err}`,
  sessionRestoreHint: ({ n }: { n: number }) => `${n} SSH tab(s) from last time — reconnect from the session library`,

  // ---------- backend error codes ----------
  errSessionNotFound: 'Session not found or already closed',
  errAuthFailed: 'Authentication failed — check the user name and password',
  errKeyAuthFailed: 'Key authentication failed — check the key and passphrase',
  errJumpAuthFailed: 'Jump host authentication failed',
  errTargetAuthFailed: 'Target host authentication failed',
  errCancelled: 'Transfer cancelled',
  errConnect: ({ err }: { err: string }) => `Cannot reach the host: ${err}`,
  errHostKeyChanged: ({ line }: { line: string }) => `Host key changed (known_hosts line ${line})! This may indicate a man-in-the-middle attack; the connection was aborted. Verify the host and update the record manually.`,
  errHostKeyUnknown: 'Host key is not recorded and the policy is "strict", so the connection was rejected. Add the host to known_hosts first.',
  errHostKeyNoHome: 'Cannot locate the user home directory, so known_hosts is unavailable; the connection was rejected.',
  errHostKeyIo: ({ err }: { err: string }) => `Could not read known_hosts: ${err}`,
  errXserverNotFound: 'No local X server (VcXsrv / Xming) found — install one first',
  errXserverStartFailed: ({ err }: { err: string }) => `Could not start the X server: ${err}`,
  errXserverReadyTimeout: 'X server did not become ready in time — check for a blocked dialog or a busy port',
  errXserverNotRunning: 'X server is not running',
  errXserverNoFreeDisplay: 'No free X display available',
  statusXserverStarted: ({ display }: { display: string }) => `X server started (display ${display})`,
  statusXserverStopped: 'X server stopped',
  errUnknown: ({ err }: { err: string }) => `Operation failed: ${err}`,
};

const dicts: Record<Lang, Dict> = { zh, en };

const LANG_KEY = 'rustterm.lang';

export function loadLang(): Lang {
  const raw = localStorage.getItem(LANG_KEY);
  if (raw === 'en' || raw === 'zh') return raw;
  // 首次启动跟随系统语言，中文环境默认中文。
  return (navigator.language || '').toLowerCase().startsWith('zh') ? 'zh' : 'en';
}

export function saveLang(lang: Lang): void {
  localStorage.setItem(LANG_KEY, lang);
}

/** 取当前语言下的一条文案；缺失时回退到中文，再回退到键名本身。 */
export function makeT(lang: Lang) {
  const dict = dicts[lang];
  return function t(key: string, params?: Record<string, unknown>): string {
    const entry = dict[key] ?? dicts.zh[key];
    if (entry === undefined) return key;
    return typeof entry === 'function' ? entry(params ?? {}) : entry;
  };
}

export type T = ReturnType<typeof makeT>;

/**
 * 把 static 文案写到所有带 data-i18n 的节点上。
 * 同时负责 title / placeholder / aria-label，避免 HTML 里再写死文案。
 */
export function applyI18n(t: T): void {
  document.querySelectorAll<HTMLElement>('[data-i18n]').forEach((el) => {
    const key = el.dataset.i18n;
    if (key) el.textContent = t(key);
  });
  document.querySelectorAll<HTMLElement>('[data-i18n-title]').forEach((el) => {
    const key = el.dataset.i18nTitle;
    if (key) el.title = t(key);
  });
  document.querySelectorAll<HTMLElement>('[data-i18n-placeholder]').forEach((el) => {
    const key = el.dataset.i18nPlaceholder;
    if (key) (el as HTMLInputElement).placeholder = t(key);
  });
  document.documentElement.lang = document.documentElement.dataset.lang === 'en' ? 'en' : 'zh-CN';
}

/** 把后端返回的错误码翻译成用户可读文案。 */
export function errorText(t: T, err: unknown): string {
  const raw = typeof err === 'string' ? err : err instanceof Error ? err.message : String(err);
  switch (raw) {
    case 'session-not-found':
    case 'session not found':
      return t('errSessionNotFound');
    case 'auth-failed':
      return t('errAuthFailed');
    case 'key-auth-failed':
      return t('errKeyAuthFailed');
    case 'jump-auth-failed':
      return t('errJumpAuthFailed');
    case 'target-auth-failed':
      return t('errTargetAuthFailed');
    case 'cancelled':
    case '传输已取消':
      return t('errCancelled');
    case 'xserver-not-found':
      return t('errXserverNotFound');
    case 'xserver-not-running':
      return t('errXserverNotRunning');
    case 'xserver-ready-timeout':
      return t('errXserverReadyTimeout');
    case 'xserver-no-free-display':
      return t('errXserverNoFreeDisplay');
    default: {
      // 主机密钥类错误带冒号分隔的附加信息，单独解析
      if (raw.startsWith('host-key-changed:')) {
        return t('errHostKeyChanged', { line: raw.slice('host-key-changed:'.length) });
      }
      if (raw === 'host-key-unknown') return t('errHostKeyUnknown');
      if (raw === 'host-key-no-home') return t('errHostKeyNoHome');
      if (raw.startsWith('host-key-io:')) {
        return t('errHostKeyIo', { err: raw.slice('host-key-io:'.length) });
      }
      // 凭据库相关
      if (raw === 'secret-password-required') return t('secretPasswordRequired');
      if (raw.startsWith('secret-store-unavailable:')) {
        return t('secretStoreUnavailable', { detail: raw.slice('secret-store-unavailable:'.length) });
      }
      if (raw.startsWith('secret-write-failed:')) {
        return t('secretStoreUnavailable', { detail: raw.slice('secret-write-failed:'.length) });
      }
      if (raw.startsWith('secret-read-failed:')) {
        return t('secretStoreUnavailable', { detail: raw.slice('secret-read-failed:'.length) });
      }
      if (raw.startsWith('secret-delete-failed:')) {
        return t('secretStoreUnavailable', { detail: raw.slice('secret-delete-failed:'.length) });
      }
      if (raw.startsWith('xserver-spawn-failed:')) {
        return t('errXserverStartFailed', { err: raw.slice('xserver-spawn-failed:'.length) });
      }
      // 连接类错误由 russh/io 直接抛出，原文是英文技术描述，
      // 统一加一层本地化前缀，用户至少知道是网络环节出了问题。
      if (/connect|connection|timed? ?out|refused|unreachable|dns|resolve/i.test(raw)) {
        return t('errConnect', { err: raw });
      }
      return raw;
    }
  }
}