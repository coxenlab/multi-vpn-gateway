// 添加通道弹层（界面 09–14）。拖文件或选文件 → sniff.js 识别 → 按结论分派；没有文件就选类型填表。
// 第 15 步（正在连接）在 add-progress.js：点「添加并连接」后调 mount(sheet, {...ctx, draft})，把弹层句柄和填好的内容交给它。
// 弹层内是多步本地状态，File 对象放不进地址栏。
import { h, fill, btn, verdict, ic } from '../ui.js';
import { openSheet } from '../sheet.js';
import { api, isAbort } from '../api.js';
import { humanize, clean } from '../feedback.js';
import { typeLabel, FORBIDDEN } from '../terms.js';
import { HEAD_BYTES, TAIL_BYTES, precheck, sniff, sizeProblem, describe, conclusion } from '../sniff.js';

// ---- 纯函数（node 可直接导入测试） ----

// 类型网格先放这几种，其余收在「更多…」里
export const PRIMARY = Object.freeze(['custom', 'easyconnect', 'atrust', 'openvpn', 'wireguard', 'anyconnect']);

export function splitTypes(list = []) {
  // 保留已有预装通道的运行支持；新建统一从官方安装包进入。
  const available = list.filter(a => a.key !== 'hillstone');
  const byKey = new Map(available.map(a => [a.key, a]));
  const primary = PRIMARY.map(k => byKey.get(k)).filter(Boolean);
  const rest = available.filter(a => !PRIMARY.includes(a.key));
  return { primary, rest };
}

const LABEL = { server: '服务器', username: '账号', password: '密码', config_file: '配置文件', package: '安装包' };
export const fieldLabel = input => LABEL[input.key] || String(input.label || input.key).replace(/[(（][^)）]*[)）]\s*$/, '').trim();

const optional = input => /可选/.test(String(input.label || ''));

// 只显示这种类型必须填的项：标了 required 的，加上没标「可选」的文字项（EasyConnect、aTrust 的清单里没有标 required）。
// 其余可选项放进「更多」。文件项由拖进来的文件填上，不单独占一行。
export function formInputs(adapter) {
  const main = [], more = [];
  for (const input of (adapter && adapter.inputs) || []) {
    if (input.required || (input.type !== 'file' && !optional(input))) main.push(input);
    else more.push(input);
  }
  return { main, more };
}

// 配置文件的上限。它读成文本放进请求体，核心收请求体的上限是 2 MB（框架默认值），转义后还会变长，所以只收 1 MB
export const CONFIG_LIMIT = 1024 * 1024;
// 超过上限的配置文件不读，直接提示；没超过返回 null
export function configProblem(file) {
  if (!file || !(file.size > CONFIG_LIMIT)) return null;
  return { code: 'too-big', title: '配置文件太大了', text: '连接用的配置文件一般只有几 KB，不能超过 1 MB。换一个文件试试。' };
}
const hasConfigInput = adapter => ((adapter && adapter.inputs) || []).some(i => i.type === 'file' && i.key === 'config_file');

// 本机能用的最新版本：列表已经按版本号从新到旧排好（DESIGN 5.10）。拿不到列表时留空，由后台用默认版本
export function pickVersion(versions) {
  const list = Array.isArray(versions) ? versions : [];
  const v = list.find(x => x && x.usable_here) || list[0];
  return v ? String(v.tag) : '';
}

const modesOf = adapter => (adapter && Array.isArray(adapter.login_modes) ? adapter.login_modes : []);
// 登录时能在窗口里再输密码
export const loginInWindow = adapter => modesOf(adapter).some(m => m === 'gui' || m === 'byo');

// 登录方式由类型决定，不让用户选：只能自动登录的 headless，自带客户端的 byo；
// 两种都行的（EasyConnect）填了密码就自动登录，没填就在窗口里登录
export function loginMethod(adapter, password) {
  const modes = modesOf(adapter);
  if (modes.length === 1 && modes[0] === 'headless') return 'headless';
  if (modes.length === 1 && modes[0] === 'byo') return 'byo';
  if (modes.includes('headless') && password) return 'password';
  return 'interactive';
}

// 提交前的检查：返回第一句要提示的话，没有问题返回 null
export function missing(adapter, { name, values = {} } = {}) {
  if (!String(name || '').trim()) return '给它起个名字';
  for (const input of (adapter && adapter.inputs) || []) {
    if (!input.required) continue;
    const v = values[input.key];
    if (input.type === 'file' ? !v : !String(v ?? '').trim()) return input.type === 'file' ? `请选择${fieldLabel(input)}` : `请填写${fieldLabel(input)}`;
  }
  return null;
}

// 交给第 15 步（add-progress.js）的内容：文字项放 fields，文件项（配置文件或安装包）放 file，
// 请求体由 add-progress.js 拼（它把配置文件读成文本放进 config，安装包建好通道后另外上传）
export function makeDraft(adapter, { name, values = {}, target = '', version = '' } = {}) {
  const fields = {};
  let file = null;
  for (const input of (adapter && adapter.inputs) || []) {
    const v = values[input.key];
    if (input.type === 'file') { if (v) file = v; continue; }
    if (v == null || v === '') continue;
    fields[input.key] = input.type === 'password' ? String(v) : String(v).trim();
  }
  return {
    name: String(name || '').trim(),
    vpn_type: adapter.key,
    ec_ver: adapter.versioned ? version : '',
    login_method: loginMethod(adapter, fields.password),
    probe_url: String(target || '').trim(), // wording-allow
    spec: adapter,
    fields,
    file,
  };
}

// 类型自带的说明只在不含内部用词时原样显示
export function noticeText(adapter) {
  const n = adapter && typeof adapter.notice === 'string' ? adapter.notice.trim() : '';
  if (n && !FORBIDDEN.test(n)) return n;
  return adapter && modesOf(adapter).includes('byo') && !(adapter.inputs || []).length ? '客户端已经内置，添加以后在登录窗口里登录。' : '';
}

// ---- 弹层 ----

const TARGET_HINT = '一个连上以后才能打开的内网地址（可以不填）';

export function mount(root, ctx) {
  const signal = ctx.signal;
  let types = null;        // /api/vpn-types
  let typesError = null;
  let progress = null;     // 第 15 步的视图
  let token = 0;           // 最近一次选文件，晚到的识别结果丢掉
  let pollTimer = null;

  // view：pick 选文件或类型 / file 识别结果 / form 填写连接信息 / go 正在连接
  const st = { view: 'pick', problem: null, file: null, result: null, adapter: null, values: {}, name: '', target: '', more: false, error: '', back: 'pick', showRest: false, image: null, busy: false };

  const picker = h('input', { type: 'file', hidden: true, 'aria-hidden': 'true', tabindex: '-1' });
  picker.addEventListener('change', () => {
    const files = Array.from(picker.files || []);
    picker.value = '';
    if (files.length) takeFiles({ files, folders: 0 });
  });

  const sheet = openSheet({
    title: '添加通道', width: 'w680', root,
    body: [],
    foot: [],
    onConfirm: () => { if (primaryAction && !st.busy) primaryAction(); },
    onClose: () => { cleanup(); ctx.close(); },
  });
  sheet.el.classList.add('add');
  sheet.el.append(picker);

  let primaryAction = null;
  const choose = () => picker.click();

  function cleanup() {
    resetImage();
    if (progress && progress.unmount) { try { progress.unmount(); } catch (e) { console.error('[add-progress]', e); } }
    progress = null;
  }

  // ---- 取数 ----

  function loadTypes() {
    typesError = null;
    api.vpnTypes({ signal }).then(list => {
      types = Array.isArray(list) ? list : [];
      // 类型清单晚于识别结果到达：补上识别出的类型
      if (st.view === 'file' && st.result && st.result.type && !st.adapter) {
        st.adapter = adapterOf(st.result.type);
        if (st.adapter) { attachFile(st.adapter, st.file); loadVersions(st.adapter); }
      }
      render();
    }, e => {
      if (isAbort(e)) return;
      typesError = e;
      render();
    });
  }

  const adapterOf = key => (types || []).find(a => a.key === key) || null;

  let versions = null;
  function loadVersions(adapter) {
    versions = adapter && adapter.versioned
      ? api.vpnVersions(adapter.key, { signal }).then(r => (r && r.versions) || [], () => [])
      : Promise.resolve([]);
  }

  // ---- 文件 ----

  async function takeFiles(payload) {
    if (st.view === 'go') return;
    const my = ++token;
    const problem = precheck(payload || {});
    if (problem) { Object.assign(st, { view: 'pick', problem, file: null, result: null }); render(); return; }
    const file = payload.files[0];
    let head, tail;
    try {
      head = new Uint8Array(await file.slice(0, HEAD_BYTES).arrayBuffer());
      tail = file.size >= TAIL_BYTES ? new Uint8Array(await file.slice(file.size - TAIL_BYTES).arrayBuffer()) : new Uint8Array(0);
    } catch {
      if (my !== token) return;
      Object.assign(st, { view: 'pick', problem: { title: '读不了这个文件', text: '换一个文件试试。' } });
      render();
      return;
    }
    if (my !== token) return;
    const result = sniff({ name: file.name, size: file.size, head, tail });
    const tooBig = sizeProblem(result, file.size) || ((result.kind === 'wireguard' || result.kind === 'openvpn') && configProblem(file));
    if (tooBig) { Object.assign(st, { view: 'pick', problem: tooBig, file: null, result: null }); render(); return; }
    resetImage();
    Object.assign(st, { view: 'file', problem: null, file, result, error: '', more: false, values: {}, back: 'pick' });
    st.adapter = result.type ? adapterOf(result.type) : null;
    if (st.adapter) attachFile(st.adapter, file);
    loadVersions(st.adapter);
    render();
  }

  // 把文件放进该类型的文件项（配置文件、安装包）
  function attachFile(adapter, file) {
    const input = (adapter.inputs || []).find(i => i.type === 'file');
    if (input && file) st.values[input.key] = file;
    return input ? input.key : null;
  }

  function resetImage() {
    clearTimeout(pollTimer);
    if (st.image && st.image.id && st.image.status === 'preview') api.imageImportDiscard(st.image.id).catch(() => {});
    st.image = null;
  }

  // ---- 类型 ----

  function pickType(key) {
    const adapter = adapterOf(key);
    if (!adapter) return;
    const fromFile = st.view === 'file' ? 'file' : 'pick';
    if (fromFile === 'file' && st.result && st.result.verdict === 'unknown' && hasConfigInput(adapter)) {
      const big = configProblem(st.file);
      if (big) { st.error = `${big.title}。${big.text}`; render(); return; }
    }
    st.values = {};
    if (fromFile === 'file' && st.result && st.result.verdict === 'unknown') attachFile(adapter, st.file);
    Object.assign(st, { view: 'form', adapter, error: '', more: false, back: fromFile });
    loadVersions(adapter);
    render();
  }

  // ---- 提交：交给第 15 步 ----

  async function submit() {
    const adapter = st.adapter;
    if (!adapter || st.busy) return;
    const err = missing(adapter, { name: st.name, values: st.values });
    if (err) { st.error = err; render(); return; }
    st.busy = true;
    render();
    const version = adapter.versioned ? pickVersion(await versions) : '';
    if (signal.aborted) return;
    const draft = makeDraft(adapter, { name: st.name, values: st.values, target: st.target, version });
    let mod;
    try { mod = await import('./add-progress.js'); } catch (e) {
      console.error('[add-progress]', e);
      st.busy = false;
      st.error = '下一步没能打开，稍后再试一次';
      render();
      return;
    }
    if (signal.aborted) return;
    st.view = 'go';
    primaryAction = null;
    sheet.el.classList.remove('w680');
    sheet.setTitle('正在连接 ' + draft.name, '');
    sheet.setFoot();
    fill(sheet.body);
    try {
      progress = mod.mount(sheet, { ...ctx, draft }) || null;
    } catch (e) {
      console.error('[add-progress]', e);
    }
  }

  // ---- 客户端离线包：先校验预览，再确认导入 ----

  async function importPreview() {
    st.busy = true; st.error = ''; render();
    try {
      const r = await api.imageImport(st.file, { signal });
      st.image = { id: r.id, status: r.status, preview: r.preview, progress: r.progress };
    } catch (e) {
      if (isAbort(e)) return;
      st.error = message(e, '没能校验这个离线包');
    }
    st.busy = false;
    render();
  }

  async function importConfirm() {
    const img = st.image;
    if (!img) return;
    st.busy = true; st.error = ''; render();
    try {
      const r = await api.imageImportConfirm(img.id, { signal });
      img.status = r.status || 'loading';
      if (img.status === 'loading') { pollImport(); return; }
      Object.assign(img, r);
    } catch (e) {
      if (isAbort(e)) return;
      st.error = message(e, '没能导入这个离线包');
    }
    st.busy = false;
    render();
  }

  function pollImport() {
    clearTimeout(pollTimer);
    pollTimer = setTimeout(async () => {
      const img = st.image;
      if (!img || signal.aborted) return;
      try {
        const r = await api.imageImportStatus(img.id, { signal });
        Object.assign(img, { status: r.status, progress: r.progress, error: r.error });
      } catch (e) {
        if (isAbort(e)) return;
        img.status = 'error';
        img.error = '';
      }
      if (img.status === 'loading') { render(); pollImport(); return; }
      st.busy = false;
      render();
    }, 1500);
  }

  const message = (e, fallback) => {
    const m = humanize(e, { fallback });
    return m ? (m.text ? `${m.title}。${m.text}` : m.title) : fallback;
  };

  // ---- 渲染 ----

  function render() {
    if (st.view === 'go') return;
    primaryAction = null;
    // 有整张类型网格的两步用宽弹层
    sheet.el.classList.toggle('w680', st.view === 'pick' || (st.view === 'file' && st.result.verdict === 'unknown'));
    if (st.view === 'form') {
      sheet.setTitle('添加通道', typeLabel(st.adapter.key, st.adapter.label));
      sheet.setBody(formView());
    } else {
      sheet.setTitle('添加通道', '');
      sheet.setBody(st.view === 'file' ? fileView() : pickView());
    }
    sheet.setFoot(foot());
    // 没有输入框的一步：焦点放在弹层本身，回车走这一步的主按钮，而不是按到「换一个」
    if (!sheet.body.querySelector('input:not([type=file])')) { sheet.el.tabIndex = -1; sheet.el.focus(); }
  }

  function pickView() {
    const drop = h('div', { class: 'drop' },
      h('span', { class: 'ic' }, ic('tray', 30, 1.3)),
      h('b', null, '把安装包或配置文件拖到这里'),
      h('span', null, '或者 ', h('button', { type: 'button', class: 'link', onClick: choose }, '选择文件'), '。我们会自动识别并选好运行方式。'));
    return [
      drop,
      st.problem ? verdict('bad', { title: st.problem.title, text: st.problem.text }) : null,
      h('div', { class: 'ft' }, '没有文件？选择类型'),
      typeGrid(),
    ];
  }

  function typeGrid({ all = false } = {}) {
    if (typesError) {
      return verdict('bad', { title: '没能读取通道类型', text: message(typesError, '没能读取通道类型'), action: btn('重试', { cls: 'sm', icon: 'refresh', onClick: () => { typesError = null; render(); loadTypes(); } }) });
    }
    if (!types) return h('div', { class: 'types' }, Array.from({ length: 8 }, () => h('div', { class: 'type add-skel', 'aria-hidden': 'true' })));
    const { primary, rest } = splitTypes(types);
    const shown = all || st.showRest ? [...primary, ...rest] : primary;
    const cells = shown.map(a => h('button', { type: 'button', class: 'type', 'data-type': a.key, onClick: () => pickType(a.key) },
      h('span', { class: 'g' }), h('span', { class: 'cell', title: typeLabel(a.key, a.label) }, typeLabel(a.key, a.label))));
    if (!all && !st.showRest && rest.length) {
      cells.push(h('button', { type: 'button', class: 'type more', onClick: () => { st.showRest = true; render(); } }, '更多…'));
    }
    return h('div', { class: 'types' }, cells);
  }

  function fileCard() {
    return h('div', { class: 'file' },
      h('div', { class: 'doc' }, ic('doc', 22, 1.3)),
      h('div', { class: 'two' }, h('span', { class: 'mono', title: st.file.name }, st.file.name), h('small', null, describe(st.result, st.file.name, st.file.size))),
      btn('换一个', { cls: 'ghost sm', onClick: choose }));
  }

  function fileView() {
    const c = conclusion(st.result);
    const kids = [fileCard(), verdict(c.tone, { title: c.title, text: c.text })];
    if (c.next) {
      kids.push(h('div', { class: 'ft' }, '你可以'), h('div', { class: 'form' }, c.next.map(t => h('div', { class: 'fr' }, t))));
    } else if (st.result.kind === 'image') {
      kids.push(imageBlock());
    } else if (st.result.verdict === 'unknown') {
      kids.push(h('div', { class: 'ft' }, '它属于哪种类型'), typeGrid({ all: true }));
    } else if (st.adapter) {
      kids.push(h('div', { class: 'ft' }, '给它起个名字'), nameField('add-name'));
      const { main, more } = formInputs(st.adapter);
      const rows = main.filter(i => i.type !== 'file').map(inputRow);
      if (rows.length) kids.push(h('div', { class: 'form add-gap' }, rows));
      kids.push(...moreBlock(more.filter(i => st.values[i.key] == null || i.type !== 'file')));
    } else if (st.result.type) {
      kids.push(typesError ? typeGrid() : h('div', { class: 'ft' }, '正在读取通道类型…'));
    }
    if (st.error) kids.push(h('div', { class: 'add-err', role: 'alert' }, st.error));
    return kids;
  }

  function imageBlock() {
    const img = st.image;
    if (!img) return null;
    const labels = [...new Set(((img.preview && img.preview.images) || []).flatMap(i => i.adapters || []))]
      .map(k => typeLabel(k, (adapterOf(k) || {}).label));
    const kids = [];
    if (labels.length) kids.push(h('div', { class: 'ft' }, '里面的客户端'), h('div', { class: 'form' }, labels.map(t => h('div', { class: 'fr' }, t))));
    if (img.status === 'loading') kids.push(verdict('info', { title: '正在导入', text: '离线包比较大时要等几分钟，可以一直开着这个窗口。' }));
    if (img.status === 'done') kids.push(verdict('ok', { title: '已导入', text: '添加对应类型的通道时不用再下载。' }));
    if (img.status === 'error') kids.push(verdict('bad', { title: '导入没有完成', text: clean(img.error) || '稍后再试一次。' }));
    return kids;
  }

  function formView() {
    const adapter = st.adapter;
    const { main, more } = formInputs(adapter);
    const notice = noticeText(adapter);
    return [
      notice ? verdict('info', { title: '', text: notice }) : null,
      h('div', { class: 'form' + (notice ? ' add-gap' : '') }, h('div', { class: 'fr' }, h('label', null, '名字'), nameField('grow')), main.map(inputRow)),
      ...moreBlock(more),
      st.error ? h('div', { class: 'add-err', role: 'alert' }, st.error) : null,
    ];
  }

  function nameField(cls) {
    const el = h('input', { class: cls === 'add-name' ? 'field add-name' : 'grow', type: 'text', placeholder: '例如 园区办公', 'aria-label': '名字', autocomplete: 'off', spellcheck: 'false', autofocus: true });
    el.value = st.name;
    el.addEventListener('input', () => { st.name = el.value; if (st.error) clearError(); });
    return el;
  }

  function clearError() {
    st.error = '';
    const e = sheet.body.querySelector('.add-err');
    if (e) e.remove();
  }

  function inputRow(input) {
    const label = h('label', null, fieldLabel(input));
    if (input.type === 'file') {
      const f = st.values[input.key];
      const name = h('span', { class: 'grow cell' + (f ? ' mono' : ' dim3'), title: f ? f.name : '' }, f ? f.name : '还没有选择');
      const pickOne = h('input', { type: 'file', hidden: true });
      pickOne.addEventListener('change', () => {
        const got = pickOne.files && pickOne.files[0];
        pickOne.value = '';
        const big = got && input.key === 'config_file' ? configProblem(got) : null;
        if (big) { st.error = `${big.title}。${big.text}`; render(); return; }
        if (got) { st.values[input.key] = got; clearError(); render(); }
      });
      return h('div', { class: 'fr' }, label, name, pickOne, btn(f ? '换一个' : '选择文件', { cls: 'ghost sm', onClick: () => pickOne.click() }));
    }
    const pwd = input.type === 'password';
    const el = h('input', {
      class: 'grow', type: pwd ? 'password' : 'text', 'aria-label': fieldLabel(input), autocomplete: 'off', spellcheck: 'false',
      placeholder: pwd && loginInWindow(st.adapter) ? '可以不填，登录时再输' : input.key === 'server' ? '例如 vpn.example.com' : '',
    });
    el.value = st.values[input.key] ?? '';
    el.addEventListener('input', () => { st.values[input.key] = el.value; if (st.error) clearError(); });
    return h('div', { class: 'fr' }, label, el);
  }

  // 「更多」：可以不填的内网验证地址，加上这种类型的可选项
  function moreBlock(extra) {
    const body = h('div', { class: 'add-more', hidden: !st.more });
    for (const input of extra) {
      if (input.type === 'file') { body.append(h('div', { class: 'form add-gap' }, inputRow(input))); continue; }
      body.append(h('div', { class: 'ft' }, fieldLabel(input) + '（可以不填）'), inputField(input));
    }
    const target = h('input', { class: 'field add-name', type: 'text', placeholder: '例如 oa.corp.example', 'aria-label': TARGET_HINT, autocomplete: 'off', spellcheck: 'false' });
    target.value = st.target;
    target.addEventListener('input', () => { st.target = target.value; });
    body.append(h('div', { class: 'ft' }, TARGET_HINT), target);
    const chev = h('span', { class: 'add-chev' }, ic(st.more ? 'down' : 'chev', 12, 1.8));
    const row = h('button', { type: 'button', class: 'morerow' + (st.more ? ' open' : ''), 'aria-expanded': String(st.more) }, chev, '更多');
    row.addEventListener('click', () => {
      st.more = !st.more;
      body.hidden = !st.more;
      row.classList.toggle('open', st.more);
      row.setAttribute('aria-expanded', String(st.more));
      fill(chev, ic(st.more ? 'down' : 'chev', 12, 1.8));
      if (st.more) target.focus();
    });
    return [row, body];
  }

  function inputField(input) {
    const pwd = input.type === 'password';
    const el = h('input', { class: 'field add-name', type: pwd ? 'password' : 'text', 'aria-label': fieldLabel(input), autocomplete: 'off', spellcheck: 'false' });
    el.value = st.values[input.key] ?? '';
    el.addEventListener('input', () => { st.values[input.key] = el.value; });
    return el;
  }

  function foot() {
    const cancel = btn('取消', { onClick: () => sheet.close() });
    const sp = h('span', { class: 'sp' });
    if (st.view === 'pick') return [sp, cancel];
    if (st.view === 'form') {
      const back = btn('上一步', { cls: 'ghost', onClick: () => { st.error = ''; st.view = st.back === 'file' && st.file ? 'file' : 'pick'; render(); } });
      return [back, sp, cancel, submitButton()];
    }
    const r = st.result;
    const other = btn('换一个文件', { onClick: choose });
    if (r.verdict === 'no') {
      primaryAction = () => sheet.close();
      return [sp, other, btn('好', { cls: 'pri', onClick: () => sheet.close() })];
    }
    if (r.kind === 'image') return [sp, ...imageFoot(cancel)];
    if (r.verdict === 'unknown' || !st.adapter) return [sp, other, cancel];
    return [sp, cancel, submitButton()];
  }

  function submitButton() {
    // 自定义客户端要先在窗口里手动安装
    const text = st.adapter && st.adapter.key === 'custom' ? '添加并打开窗口' : '添加并连接';
    primaryAction = submit;
    return btn(st.busy ? '正在准备…' : text, { cls: 'pri', disabled: st.busy, onClick: submit });
  }

  function imageFoot(cancel) {
    const img = st.image;
    if (!img) {
      primaryAction = importPreview;
      return [cancel, btn(st.busy ? '正在校验…' : '导入', { cls: 'pri', disabled: st.busy, onClick: importPreview })];
    }
    if (img.status === 'preview') {
      primaryAction = importConfirm;
      return [cancel, btn(st.busy ? '正在导入…' : '确认导入', { cls: 'pri', disabled: st.busy, onClick: importConfirm })];
    }
    if (img.status === 'loading') return [btn('正在导入…', { cls: 'pri', disabled: true })];
    const kinds = [...new Set(((img.preview && img.preview.images) || []).flatMap(i => i.adapters || []))].filter(adapterOf);
    const done = btn('好', { cls: kinds.length === 1 && img.status === 'done' ? '' : 'pri', onClick: () => sheet.close() });
    if (img.status === 'done' && kinds.length === 1) {
      const k = kinds[0];
      const add = btn(`添加${typeLabel(k, adapterOf(k).label)}通道`, { cls: 'pri', onClick: () => { resetImage(); st.file = null; st.view = 'pick'; pickType(k); } });
      primaryAction = () => add.click();
      return [done, add];
    }
    primaryAction = () => sheet.close();
    return [done];
  }

  loadTypes();
  if (ctx.files) takeFiles(ctx.files); else render();

  return {
    update() {},
    // 弹层已经打开时又拖进来文件（main.js 调用）
    files(payload) { takeFiles(payload); },
    unmount() { cleanup(); sheet.close(); },
  };
}

