// 拖文件识别（DESIGN 5.4）。只看文件头（≤4096 字节）和文件尾（≤512 字节），不解包、不执行。
// 结论只由魔数决定；扩展名只在魔数已经一致时用来细化说明，所以扩展名改错，结论不变。
// 纯函数，顶层不碰 document / window，node 直接导入测试。

export const HEAD_BYTES = 4096;
export const TAIL_BYTES = 512;
export const PACKAGE_LIMIT = 1024 * 1024 * 1024;        // 安装包和配置文件：1 GiB（上传接口的上限）
export const ARCHIVE_LIMIT = 12 * 1024 * 1024 * 1024;   // 客户端离线包：12 GiB（导入接口的上限）

const GiB = 1024 * 1024 * 1024;

// 读文件头之前的检查：一次拖多个、文件夹、空文件、超过所有上限。
// 返回 null 表示可以继续读文件头，否则返回 {code, title, text}
export function precheck({ files = [], folders = 0 } = {}) {
  if (folders > 0) return { code: 'folder', title: '这是一个文件夹', text: '请把安装包或配置文件本身拖进来，不要拖整个文件夹。' };
  if (files.length > 1) return { code: 'multiple', title: '一次只能添加一个文件', text: '把要用的那一个拖进来就行。' };
  if (!files.length) return { code: 'none', title: '没有拿到文件', text: '再拖一次，或者点「选择文件」。' };
  const f = files[0];
  if (!f.size) return { code: 'empty', title: '这个文件是空的', text: '检查一下文件是不是没下载完整。' };
  if (f.size > ARCHIVE_LIMIT) return { code: 'too-big', title: '文件太大了', text: '安装包和配置文件不能超过 1 GiB，客户端离线包不能超过 12 GiB。' };
  return null;
}

const at = (buf, off, bytes) => bytes.every((b, i) => buf[off + i] === b);
const ascii = (buf, off, s) => at(buf, off, Array.from(s, c => c.charCodeAt(0)));
const ext = name => { const m = /\.([a-z0-9]+)$/i.exec(String(name || '')); return m ? m[1].toLowerCase() : ''; };

function text(buf) {
  if (buf.includes(0)) return null;          // 有 NUL 就不是文本
  try { return new TextDecoder('utf-8', { fatal: false }).decode(buf); } catch { return null; }
}

const manual = kind => ({ kind, verdict: 'manual', type: 'custom' });

// 文件内容决定运行方式。厂商名称不改变安装流程，也不代表客户端已安装。
export function sniff(input = {}) {
  return detect(input);
}

function detect({ head = new Uint8Array(0), tail = new Uint8Array(0) } = {}) {
  const h = head.subarray(0, HEAD_BYTES);
  // 苹果磁盘映像：签名 koly 在最后 512 字节的开头
  if (tail.length >= TAIL_BYTES && ascii(tail, tail.length - TAIL_BYTES, 'koly')) return { kind: 'dmg', verdict: 'no' };
  if (ascii(h, 0, 'MZ')) return { kind: 'exe', verdict: 'no' };
  if (at(h, 0, [0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1])) return { kind: 'msi', verdict: 'no' };
  if (ascii(h, 0, 'xar!')) return { kind: 'pkg', verdict: 'no' };
  if (at(h, 0, [0x1f, 0x8b]) || ascii(h, 257, 'ustar')) return { kind: 'image', verdict: 'ok' };
  if (ascii(h, 0, '!<arch>\n')) return manual('deb');
  if (at(h, 0, [0xed, 0xab, 0xee, 0xdb])) return { kind: 'rpm', verdict: 'no' };
  if (at(h, 0, [0x7f, 0x45, 0x4c, 0x46]) && ascii(h, 8, 'AI') && (h[10] === 1 || h[10] === 2)) return manual('appimage');
  if (ascii(h, 0, '#!')) return manual('script');
  const t = text(h);
  if (t != null) {
    if (/\[Interface\]/.test(t) && /PrivateKey/.test(t)) return { kind: 'wireguard', verdict: 'ok', type: 'wireguard' };
    if (/(^|\n)[ \t]*remote[ \t]/.test(t) || t.includes('<ca>')) return { kind: 'openvpn', verdict: 'ok', type: 'openvpn' };
  }
  return { kind: 'unknown', verdict: 'unknown' };
}

// 读完文件头以后再按类别核对大小：只有离线包可以超过 1 GiB
export function sizeProblem(result, size) {
  if (result.kind === 'image' || !(size > PACKAGE_LIMIT)) return null;
  return { code: 'too-big', title: '文件超过 1 GiB', text: '安装包和配置文件不能超过 1 GiB。' };
}

// 文件卡片上的说明。扩展名只用来细化同一类里的说法
export function describe(result, name, bytes) {
  const e = ext(name);
  switch (result.kind) {
    case 'wireguard': return 'WireGuard 配置文件';
    case 'openvpn': return 'OpenVPN 配置文件';
    case 'image': return '客户端离线包';
    case 'deb': case 'rpm': case 'appimage': return 'Linux 安装包';
    case 'script': return e === 'run' || e === 'sh' || e === 'bin' ? 'Linux 安装程序' : 'Linux 脚本';
    case 'exe': return 'Windows 版安装包';
    case 'msi': return e === 'msi' ? 'Windows 版安装包' : 'Windows 文件';
    case 'pkg': return '苹果电脑版安装包';
    case 'dmg': return '苹果电脑版磁盘映像';
    default: return size(bytes);
  }
}

function size(n) {
  if (!(n > 0)) return '认不出的文件';
  if (n >= GiB) return (n / GiB).toFixed(1) + ' GiB';
  if (n >= 1048576) return (n / 1048576).toFixed(1) + ' MB';
  return Math.max(1, Math.round(n / 1024)) + ' KB';
}

const NEXT_NO = ['向客户要一份 Linux 版', '问问客户能不能提供 OpenVPN 或 WireGuard 配置文件'];

// 识别结论 → 界面上的结论框。tone：ok / wait / bad / info；next 为「你可以」下面的几条
export function conclusion(result) {
  switch (result.verdict) {
    case 'ok':
      if (result.kind === 'image') return { tone: 'ok', title: '可以使用', text: '这是客户端的离线包。导入以后，添加对应类型的通道时不用再下载。' };
      return { tone: 'ok', title: '可以使用', text: '配置文件里已经有连接信息，起个名字就能连接。' };
    case 'manual':
      if (result.kind === 'deb') return { tone: 'wait', title: '在独立环境中安装', text: '会安装所需依赖，并执行安装包中的官方步骤，完成后打开登录窗口。登录后再用内网地址验证连接；无需先选择厂商。' };
      return { tone: 'wait', title: '在独立环境中安装', text: '安装包会放入通道的桌面，在窗口里按官方步骤安装并登录。安装完成后，再用内网地址验证连接；无需先选择厂商。' };
    case 'no': {
      if (result.kind === 'rpm') return {
        tone: 'bad', title: '请使用 Debian 版安装包',
        text: '这个 RPM 包需要其他 Linux 发行版。当前通道使用 Debian，请提供厂商的 .deb 包或通用 Linux 安装程序。',
      };
      const mac = result.kind === 'pkg' || result.kind === 'dmg';
      return {
        tone: 'bad', title: '这个安装包不能在这台电脑上用',
        text: mac ? '它是苹果电脑版，通道里只能运行 Linux 版客户端。' : '它是 Windows 版，通道里只能运行 Linux 版客户端。',
        next: NEXT_NO,
      };
    }
    default:
      return { tone: 'info', title: '认不出这个文件', text: '选一下它属于哪种类型。' };
  }
}
