// 界面用词：全应用只用这一套。纯数据和纯函数，顶层不碰 document / window。

// 界面文字里不能出现的内部用词（不分大小写）。wording.test.mjs 和 feedback.js 共用。
export const FORBIDDEN = /mihomo|docker|socks|probe|容器/i; // wording-allow

// 通道的显示状态（derive.displayState 的结果）→ 文字和状态点颜色
export const STATE = Object.freeze({
  connected: { text: '已连接', kind: 'ok' },
  waiting: { text: '待登录', kind: 'wait' },
  unverified: { text: '已启动，还没验证', kind: 'wait' },
  error: { text: '出错', kind: 'bad' },
  stopped: { text: '已停止', kind: 'off' },
  unknown: { text: '状态待确认', kind: 'wait' },
});
export const stateText = s => (STATE[s] || STATE.waiting).text;
export const stateKind = s => (STATE[s] || STATE.waiting).kind;

// 规则「现在」一列（derive.ruleFate 的结果；inactive 是域名规则没有生效时 rules.fateFor 给的）。cls 对应 .fate 的变体
export const FATE = Object.freeze({
  disabled: { text: '已停用', cls: '' },
  direct: { text: '走原网络', cls: '' },
  'direct-unrouted': { text: '走原网络（不参与分流）', cls: '' },
  'direct-stopped': { text: '走原网络（通道已停止）', cls: '' },
  blocked: { text: '暂时阻止', cls: 'block' },
  channel: { text: '走通道', cls: 'go' },
  waiting: { text: '等待登录（流量仍进通道）', cls: '' },
  inactive: { text: '还没生效', cls: '' },
  unsupported: { text: '此接管方式不支持 IPv6', cls: '' },
  unknown: { text: '状态待确认', cls: '' },
});

// 出错原因：/api/channels/:cid/cause 的 code → 一句话和动作。拿不到原因时用 unknown
export const CAUSE = Object.freeze({
  runtime_down: '运行环境没起来',
  runtime_unknown: '状态待确认',
  image_pull_failed: '客户端下载失败',
  crash_loop: '客户端启动后崩溃',
  kicked: '账号在别处登录，被踢下线',
  unknown: '连不上，原因还不清楚',
});
export const ACTION = Object.freeze({
  restart_runtime: '重新启动运行环境',
  retry: '重试',
  relogin: '重新登录',
  view_logs: '查看日志',
});
export const causeText = code => CAUSE[code] || CAUSE.unknown;
export const actionText = action => ACTION[action] || '';

// 接管网络：/api/takeover 的 action → 需要处理里的一句话和按钮。
// install_tun.tip：已经用系统代理接管时只是建议；enable_system_proxy.domains：虚拟网卡开着、域名规则还要系统代理；
// retry_takeover.foreign：系统代理指向别人的地址，重试不会覆盖它
export const TAKEOVER_ACTION = Object.freeze({
  install_tun: { text: '启用虚拟网卡以后才能接管网络', button: '启用虚拟网卡', tip: '装上虚拟网卡，不读系统代理的程序也能走通道' },
  enable_system_proxy: { text: '可以改用系统代理接管，需要你确认一次', button: '用系统代理接管', domains: '域名规则要靠系统代理处理，需要你确认一次' },
  close_other_app: { text: '另一个应用正在接管网络，先在那边关掉', button: '' },
  retry_takeover: { text: '上次接管没有成功，可以再试一次', button: '重试', foreign: '系统代理现在指向别的地址，先在系统设置里关掉它再重试' },
});
// 需要处理里接管网络那一条的标题
export const TAKEOVER_TITLE = Object.freeze({
  none: '网络还没有接管',
  domains: '域名规则还没有生效',
  failed: '接管没有生效',
  tip: '可以启用虚拟网卡',
});
export const DOMAINS_OFF = '域名规则现在还没有生效';
export const RULES_OFF = '规则现在还没有生效';
export const RETRY_FAILED = '接管还是没有生效';

// 通道类型的显示名；表里没有的用类型自己的 label
export const TYPE = Object.freeze({
  easyconnect: '深信服 EasyConnect',
  atrust: '深信服 aTrust',
  hillstone: '山石 Secure Connect',
  wireguard: 'WireGuard',
  openvpn: 'OpenVPN',
  anyconnect: '思科 AnyConnect',
  globalprotect: 'GlobalProtect',
  custom: '通用安装（官方 Linux 包）',
});
export const typeLabel = (key, label) => TYPE[key] || label || key || '';

// 常用短句
export const PHRASE = Object.freeze({
  deferred: '已保存，连接后生效',
  runtimeTitle: '运行环境还没准备好',
  runtimeText: '启动以后通道才能连接。',
  unavailable: '当前版本还不支持这一项',
  noCause: '连不上，原因还不清楚',
  devNoTakeover: '开发版不接管网络',
});
