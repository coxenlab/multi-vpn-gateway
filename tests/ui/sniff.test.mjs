// A-01、A-02：文件头识别（sniff.js）和添加通道弹层的纯函数（add.js）。
// 每种文件造一份头部样本，按正确扩展名和错误扩展名各识别一次，结论必须相同。
import test from 'node:test';
import assert from 'node:assert/strict';
import { sniff, precheck, sizeProblem, conclusion, describe, HEAD_BYTES, TAIL_BYTES, PACKAGE_LIMIT, ARCHIVE_LIMIT } from '../../app/ui/js/sniff.js';
import { splitTypes, formInputs, fieldLabel, pickVersion, loginMethod, missing, makeDraft, noticeText, PRIMARY } from '../../app/ui/js/screens/add.js';
import { FORBIDDEN } from '../../app/ui/js/terms.js';
import { readFile } from 'node:fs/promises';

const enc = s => new TextEncoder().encode(s);
const bytes = (...parts) => {
  const arr = parts.flatMap(p => (typeof p === 'string' ? Array.from(enc(p)) : Array.from(p)));
  return new Uint8Array(arr);
};
const pad = (u8, n = HEAD_BYTES) => { const out = new Uint8Array(Math.max(n, u8.length)); out.set(u8); return out; };

// tar 头：文件名，偏移 257 处 ustar
function tarHead() {
  const b = new Uint8Array(512);
  b.set(enc('manifest.json'), 0);
  b.set(enc('ustar\x0000'), 257);
  return b;
}

// 苹果磁盘映像：最后 512 字节以 koly 开头
function dmgTail() {
  const b = new Uint8Array(TAIL_BYTES);
  b.set(enc('koly'), 0);
  return b;
}

const SAMPLES = [
  { name: 'office.conf', wrong: 'office.txt', head: enc('# 示例\n[Interface]\nPrivateKey = AAAAexampleexampleexampleexampleexampleexample=\nAddress = 10.8.0.2/32\n\n[Peer]\nPublicKey = BBBBexample=\nEndpoint = vpn.example.test:51820\nAllowedIPs = 10.8.0.0/16\n'), kind: 'wireguard', verdict: 'ok', type: 'wireguard' },
  { name: 'office.ovpn', wrong: 'office.conf', head: enc('client\ndev tun\nproto udp\nremote vpn.example.test 1194\n<ca>\n-----BEGIN CERTIFICATE-----\n'), kind: 'openvpn', verdict: 'ok', type: 'openvpn' },
  { name: 'inline.ovpn', wrong: 'inline.bin', head: enc('client\n<ca>\n-----BEGIN CERTIFICATE-----\n'), kind: 'openvpn', verdict: 'ok', type: 'openvpn' },
  { name: 'corpvpn_3.2_amd64.deb', wrong: 'corpvpn.exe', head: pad(bytes('!<arch>\ndebian-binary   ')), kind: 'deb', verdict: 'manual', type: 'custom' },
  { name: 'corpvpn-3.2.x86_64.rpm', wrong: 'corpvpn.deb', head: pad(bytes([0xed, 0xab, 0xee, 0xdb, 3, 0])), kind: 'rpm', verdict: 'no' },
  { name: 'corpvpn_3.2.run', wrong: 'corpvpn.pkg', head: enc('#!/bin/sh\n# self-extracting installer\n'), kind: 'script', verdict: 'manual', type: 'custom' },
  { name: 'CorpVPN-3.2.AppImage', wrong: 'CorpVPN.dmg', head: pad(bytes([0x7f, 0x45, 0x4c, 0x46, 2, 1, 1, 0], 'AI', [2])), kind: 'appimage', verdict: 'manual', type: 'custom' },
  { name: 'CorpVPN_Setup_3.2.exe', wrong: 'CorpVPN_Setup.deb', head: pad(bytes('MZ', [0x90, 0])), kind: 'exe', verdict: 'no' },
  { name: 'CorpVPN_3.2.msi', wrong: 'CorpVPN.run', head: pad(bytes([0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1])), kind: 'msi', verdict: 'no' },
  { name: 'CorpVPN_3.2.pkg', wrong: 'CorpVPN.AppImage', head: pad(bytes('xar!', [0, 0x1c])), kind: 'pkg', verdict: 'no' },
  { name: 'CorpVPN_3.2.dmg', wrong: 'CorpVPN.ovpn', head: pad(bytes([0x78, 0xda, 0x63, 0x60])), tail: dmgTail(), kind: 'dmg', verdict: 'no' },
  { name: 'client-image.tar.gz', wrong: 'client.deb', head: pad(bytes([0x1f, 0x8b, 8, 0])), kind: 'image', verdict: 'ok' },
  { name: 'client-image.tar', wrong: 'client.conf', head: pad(tarHead()), kind: 'image', verdict: 'ok' },
];

for (const s of SAMPLES) {
  test(`识别 ${s.kind}：${s.name}，扩展名改成 ${s.wrong} 结论不变`, () => {
    const input = n => ({ name: n, size: 2048, head: s.head, tail: s.tail || new Uint8Array(0) });
    const right = sniff(input(s.name));
    const wrong = sniff(input(s.wrong));
    assert.equal(right.kind, s.kind);
    assert.equal(right.verdict, s.verdict);
    assert.equal(right.type, s.type);
    assert.deepEqual(wrong, right);
  });
}

test('A-02：已知类型可以使用，未知 Linux 包需要手动安装，Windows 和苹果安装包不能用并给出下一步', () => {
  const by = kind => conclusion(sniff({ name: 'x', head: SAMPLES.find(s => s.kind === kind).head, tail: SAMPLES.find(s => s.kind === kind).tail || new Uint8Array(0) }));
  for (const k of ['wireguard', 'openvpn', 'image']) assert.equal(by(k).title, '可以使用', k);
  for (const k of ['deb', 'script', 'appimage']) {
    assert.equal(by(k).title, '在独立环境中安装', k);
    assert.equal(by(k).tone, 'wait');
  }
  for (const k of ['exe', 'msi', 'pkg', 'dmg']) {
    const c = by(k);
    assert.equal(c.title, '这个安装包不能在这台电脑上用', k);
    assert.equal(c.tone, 'bad');
    assert.ok(c.next.length >= 1, k);
  }
  assert.match(by('exe').text, /Windows/);
  assert.match(by('dmg').text, /苹果/);
  assert.equal(by('rpm').tone, 'bad');
  assert.match(by('rpm').text, /Debian/);
});

test('认不出的文件：让用户选类型', () => {
  const r = sniff({ name: 'notes.txt', head: enc('just some text\nnothing to see\n'), tail: new Uint8Array(0) });
  assert.deepEqual(r, { kind: 'unknown', verdict: 'unknown' });
  assert.equal(conclusion(r).title, '认不出这个文件');
  // 只有 ELF、没有 AppImage 标记，也不猜
  assert.equal(sniff({ name: 'a.AppImage', head: pad(bytes([0x7f, 0x45, 0x4c, 0x46, 2, 1, 1, 0])) }).verdict, 'unknown');
  // 二进制里偶然出现 remote 字样不算配置文件
  assert.equal(sniff({ name: 'x.ovpn', head: bytes('remote x', [0, 1, 2]) }).verdict, 'unknown');
  // 空样本
  assert.equal(sniff({ name: 'x' }).verdict, 'unknown');
});

test('厂商名称不改变文件识别或绕过兼容性检查', () => {
  for (const sample of SAMPLES) {
    const expected = sniff({ name: sample.name, head: sample.head, tail: sample.tail });
    for (const vendor of ['AcmeVPN', 'Hillstone', 'SecureConnect', 'NewVendor']) {
      const result = sniff({ name: `${vendor}-${sample.name}`, head: sample.head, tail: sample.tail });
      assert.deepEqual(result, expected);
      assert.deepEqual(conclusion(result), conclusion(expected));
      assert.doesNotMatch(conclusion(result).title, /已经内置/);
    }
  }
});

test('已知和未知厂商 Linux 包都保留原文件，创建通用通道', () => {
  const adapter = { key: 'custom', login_modes: ['byo'], inputs: [{ key: 'package', type: 'file' }] };
  for (const name of ['AcmeVPN.deb', 'Hillstone.deb', 'NewVendor.deb']) {
    const file = { name, size: 123 };
    const result = sniff({ name, head: enc('!<arch>\n') });
    assert.equal(result.type, 'custom');
    assert.match(conclusion(result).text, /官方步骤/);
    const draft = makeDraft(adapter, { name: '新通道', values: { package: file } });
    assert.equal(draft.vpn_type, 'custom');
    assert.equal(draft.login_method, 'byo');
    assert.equal(draft.file, file);
  }
});

test('A-03：读文件头之前拦下空文件、文件夹、一次多个、超过上限', () => {
  const f = size => ({ name: 'a.conf', size });
  assert.equal(precheck({ files: [f(0)] }).code, 'empty');
  assert.equal(precheck({ files: [f(10)], folders: 1 }).code, 'folder');
  assert.equal(precheck({ files: [f(10), f(20)] }).code, 'multiple');
  assert.equal(precheck({ files: [] }).code, 'none');
  assert.equal(precheck({ files: [f(ARCHIVE_LIMIT + 1)] }).code, 'too-big');
  assert.equal(precheck({ files: [f(10)] }), null);
  // 1 GiB 到 12 GiB 之间：只有离线包可以
  const big = PACKAGE_LIMIT + 1;
  assert.equal(sizeProblem({ kind: 'deb' }, big).code, 'too-big');
  assert.equal(sizeProblem({ kind: 'unknown' }, big).code, 'too-big');
  assert.equal(sizeProblem({ kind: 'image' }, big), null);
  assert.equal(sizeProblem({ kind: 'deb' }, PACKAGE_LIMIT), null);
  for (const p of [precheck({ files: [f(0)] }), precheck({ files: [f(1), f(2)] }), precheck({ files: [f(1)], folders: 1 }), sizeProblem({ kind: 'deb' }, big)]) {
    assert.ok(p.title && p.text, '每种情况都有一句提示');
  }
});

test('文件卡片的说明：扩展名只细化说法', () => {
  const msi = sniff({ name: 'a.msi', head: SAMPLES.find(s => s.kind === 'msi').head });
  assert.equal(describe(msi, 'a.msi'), 'Windows 版安装包');
  assert.equal(describe(msi, 'a.doc'), 'Windows 文件');
  assert.equal(describe({ kind: 'deb' }, 'x.deb'), 'Linux 安装包');
  assert.equal(describe({ kind: 'unknown' }, 'x', 2048), '2 KB');
});

test('界面文字不含内部用词', () => {
  const all = SAMPLES.map(s => conclusion(sniff({ name: s.name, head: s.head, tail: s.tail || new Uint8Array(0) })));
  all.push(conclusion({ kind: 'unknown', verdict: 'unknown' }), conclusion({ verdict: 'manual', hint: 'vendor' }));
  for (const c of all) assert.ok(!FORBIDDEN.test(JSON.stringify(c)), JSON.stringify(c));
});

// ---- add.js 的纯函数，用真实的类型清单（夹具由 adapters.yaml 生成）----

const TYPES = JSON.parse(await readFile(new URL('./fixtures/add/api/vpn-types.json', import.meta.url), 'utf8'));
const A = key => TYPES.find(a => a.key === key);

test('类型网格：通用安装在首位，不再提供旧专用镜像入口', () => {
  const { primary, rest } = splitTypes(TYPES);
  assert.deepEqual(primary.map(a => a.key), PRIMARY);
  assert.equal(primary.length + rest.length, TYPES.filter(a => a.key !== 'hillstone').length);
  assert.equal(primary[0].key, 'custom');
  assert.ok(!rest.some(a => a.key === 'hillstone'));
});

test('A-05：只显示必须填的项。EasyConnect 服务器、账号、密码；aTrust 只有服务器', () => {
  const show = key => formInputs(A(key)).main.map(fieldLabel);
  assert.deepEqual(show('easyconnect'), ['服务器', '账号', '密码']);
  assert.deepEqual(show('atrust'), ['服务器']);
  assert.deepEqual(show('wireguard'), ['配置文件']);
  assert.deepEqual(show('openvpn'), ['配置文件']);
  assert.deepEqual(formInputs(A('openvpn')).more.map(fieldLabel), ['账号', '密码']);
  assert.deepEqual(show('hillstone'), []);
  assert.deepEqual(show('custom'), []);
  assert.deepEqual(formInputs(A('custom')).more.map(fieldLabel), ['安装包']);
});

test('A-09：版本取本机能用的最新一个；断网时核心给的回退列表也能用；没有列表时留空', () => {
  assert.equal(pickVersion([{ tag: '7.6.8', usable_here: false }, { tag: '7.6.7', usable_here: true }, { tag: '7.6.3', usable_here: true }]), '7.6.7');
  // 回退列表：arch 为空、usable_here 恒为真（dockerhub.rs 的 fallback_versions）
  assert.equal(pickVersion([{ tag: '7.6.3', arch: [], usable_here: true }, { tag: '7.6.7', arch: [], usable_here: true }]), '7.6.3');
  assert.equal(pickVersion([{ tag: '7.6.8', usable_here: false }]), '7.6.8');
  assert.equal(pickVersion([]), '');
  assert.equal(pickVersion(undefined), '');
});

test('登录方式由类型决定', () => {
  assert.equal(loginMethod(A('anyconnect'), 'x'), 'headless');
  assert.equal(loginMethod(A('wireguard')), 'headless');
  assert.equal(loginMethod(A('easyconnect'), 'secret'), 'password');
  assert.equal(loginMethod(A('easyconnect'), ''), 'interactive');
  assert.equal(loginMethod(A('atrust')), 'interactive');
  assert.equal(loginMethod(A('custom')), 'byo');
  assert.equal(loginMethod(A('hillstone')), 'byo');
});

test('提交前检查：名字和标了必填的项', () => {
  assert.equal(missing(A('easyconnect'), { name: ' ' }), '给它起个名字');
  assert.equal(missing(A('easyconnect'), { name: '华东研发' }), null);
  assert.equal(missing(A('wireguard'), { name: '办公室' }), '请选择配置文件');
  assert.equal(missing(A('wireguard'), { name: '办公室', values: { config_file: {} } }), null);
  assert.equal(missing(A('anyconnect'), { name: 'a', values: { server: 'vpn.example.test' } }), '请填写账号');
});

test('交给正在连接的内容：文字项去掉首尾空白，文件单独放，版本只给有版本的类型', () => {
  const conf = { name: 'office.conf' };
  const wg = makeDraft(A('wireguard'), { name: ' 办公室 ', values: { config_file: conf }, target: ' oa.example.test ' });
  assert.equal(wg.name, '办公室');
  assert.equal(wg.vpn_type, 'wireguard');
  assert.equal(wg.login_method, 'headless');
  assert.equal(wg.file, conf);
  assert.deepEqual(wg.fields, {});
  assert.equal(wg.probe_url, 'oa.example.test'); // wording-allow
  assert.equal(wg.ec_ver, '');
  assert.equal(wg.spec, A('wireguard'));

  const ec = makeDraft(A('easyconnect'), { name: '华东研发', values: { server: ' vpn.example.test ', username: 'zhang.wei', password: '' }, version: '7.6.7' });
  assert.deepEqual(ec.fields, { server: 'vpn.example.test', username: 'zhang.wei' });
  assert.equal(ec.login_method, 'interactive');
  assert.equal(ec.ec_ver, '7.6.7');
  assert.equal(ec.file, null);
  assert.equal(ec.probe_url, ''); // wording-allow

  const ecPwd = makeDraft(A('easyconnect'), { name: 'a', values: { password: ' p w ' } });
  assert.equal(ecPwd.login_method, 'password');
  assert.equal(ecPwd.fields.password, ' p w ', '密码原样保留');

  const pkg = { name: 'corpvpn.deb' };
  const custom = makeDraft(A('custom'), { name: '合作方', values: { package: pkg } });
  assert.equal(custom.login_method, 'byo');
  assert.equal(custom.file, pkg);
  assert.deepEqual(custom.fields, {});
});

test('类型说明含内部用词时换成通用说法', () => {
  assert.ok(!FORBIDDEN.test(noticeText(A('hillstone'))));
  assert.ok(noticeText(A('hillstone')));
  const plain = { notice: '创建通道后在登录窗口中登录客户端。' };
  assert.equal(noticeText(plain), plain.notice);
  assert.equal(noticeText(A('easyconnect')), '');
});
