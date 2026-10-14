// 设置 · 高级 · 已下载的客户端（界面 22 的二级页）：每种通道要用的客户端是否已经在这台电脑上。
// 可以按版本下载，也可以从文件导入（先校验、再确认导入）。安装命令和内部名称不上界面。
// 推导是纯函数，顶层不碰 document / window。
import { h, fill, card, lb, btn, st, pop, verdict, pageState } from '../ui.js';
import { openSheet } from '../sheet.js';
import { api, isAbort, ApiError } from '../api.js';
import { bytes } from '../format.js';
import { showError, notify, clean } from '../feedback.js';
import { pageHead, pullImage, pulling, progressText } from './settings-advanced.js';

const labels = e => (Array.isArray(e && e.used_by) ? e.used_by.filter(Boolean).map(String) : []);

// 清单 → {clients, parts}。clients 是通道客户端；parts 是分流入口自己要用的组件（只列能下载的）
export function imageGroups(inv) {
  const list = inv && Array.isArray(inv.images) ? inv.images : [];
  const clients = [], parts = [];
  for (const e of list) {
    if (!e || typeof e !== 'object') continue;
    if (e.role === 'vpn') clients.push(e);
    else if (e.role === 'infra' && e.kind === 'pull') parts.push(e);
  }
  return { clients, parts };
}

// 一种客户端的名字：用到它的通道类型，多个时连起来
export function imageTitle(e, fallback = '客户端') {
  const u = labels(e);
  return u.length ? u.join('、') : String((e && e.title) || fallback);
}

// 本机状态：present 为 true / false；按版本区分的查不出来（null）
export function presentView(e) {
  if (e && e.present === true) return { kind: 'ok', text: '已就绪' };
  if (e && e.present === false) return { kind: 'off', text: '还没下载' };
  return { kind: 'plain', text: '按版本下载' };
}

// 能在这台电脑上用的版本（按版本区分的才有）
export function usableTags(e) {
  const vs = e && Array.isArray(e.versions) ? e.versions : [];
  return vs.filter(v => v && v.tag && v.usable_here !== false).map(v => String(v.tag));
}

// 下载时的完整名称：仓库名加版本
export function pullRef(e, tag) {
  if (!e || e.kind !== 'pull') return '';
  if (e.versioned) return tag ? `${e.repo}:${tag}` : '';
  return String(e.image || (e.repo && e.tag ? `${e.repo}:${e.tag}` : ''));
}

// 导入前的校验结果 → 一段说明
export function previewText(p) {
  const imgs = p && Array.isArray(p.images) ? p.images : [];
  const kinds = [...new Set(imgs.flatMap(i => (Array.isArray(i && i.adapters) ? i.adapters : [])).map(String))];
  const size = p && Number.isFinite(Number(p.bytes)) ? bytes(p.bytes) : '';
  const head = `文件里有 ${imgs.length} 个客户端${size ? `，共 ${size}` : ''}`;
  return kinds.length ? `${head}，可以用于：${kinds.join('、')}。` : `${head}。`;
}

export function mount(root, ctx) {
  const { signal } = ctx;
  let inv = null;
  let failed = false;
  const chosen = new Map();   // 仓库名 → 选中的版本
  const state = new Map();    // 下载名 → 进度文字
  let sheet = null;

  const body = h('div', { class: 'adv-body' });
  const picker = h('input', { type: 'file', accept: '.tar,.tar.gz,.tgz,application/x-tar', hidden: true, 'aria-hidden': 'true', tabindex: '-1' });
  picker.addEventListener('change', () => {
    const f = picker.files && picker.files[0];
    picker.value = '';
    if (f) importFile(f);
  });

  fill(root, h('div', { class: 'adv' },
    pageHead('高级', '#/settings/advanced', '已下载的客户端', btn('从文件导入', { icon: 'tray', onClick: () => picker.click() })),
    h('p', { class: 'h1sub adv-lead' }, '添加通道时会自动下载要用的客户端；网络不好时可以先在这里下载，或者导入客户端离线包（.tar 或 .tar.gz 文件）。'),
    body, picker));

  function row(e, name) {
    const pv = presentView(e);
    const title = name || imageTitle(e);
    const tags = e.versioned ? usableTags(e) : [];
    const key = e.repo || e.image;
    if (e.versioned && !chosen.has(key) && tags.length) chosen.set(key, tags[0]);
    const ref = pullRef(e, chosen.get(key));
    const prog = ref ? state.get(ref) : '';
    const ctl = [];
    if (e.versioned) {
      ctl.push(tags.length
        ? pop(tags.map(t => ({ value: t, label: t })), { value: chosen.get(key), label: `${title} 的版本`, disabled: !!prog, onChange: v => { chosen.set(key, v); render(); } })
        : h('span', { class: 'dim' }, '暂时读不到版本'));
    }
    if (e.kind === 'pull' && ref) {
      ctl.push(btn(prog ? '正在下载' : e.present === true ? '重新下载' : '下载', { cls: e.present === true ? 'sm' : 'pri sm', disabled: !!prog, onClick: () => pull(ref) }));
    }
    const sub = prog || (e.kind === 'build' ? (e.present === false ? '随应用提供，缺了可以从文件导入' : '随应用提供') : '');
    return h('div', { class: 'row' },
      h('div', { class: 'two sp' }, h('span', { class: 'adv-t cell', title }, title), sub ? h('small', null, sub) : null),
      st(pv.kind, pv.text),
      ctl.length ? h('div', { class: 'adv-acts' }, ctl) : null);
  }

  function render() {
    if (!inv) { fill(body, failed ? pageState('error', { onRetry: load }) : pageState('loading')); return; }
    const g = imageGroups(inv);
    fill(body, h('div', { class: 'grid' },
      card([lb('通道客户端'), g.clients.length ? h('div', { class: 'rows' }, g.clients.map(e => row(e))) : h('div', { class: 'adv-none dim' }, '没有读到客户端清单。')]),
      g.parts.length ? card([lb('分流入口组件'), h('div', { class: 'rows' }, g.parts.map(e => row(e, '分流入口')))]) : null));
  }

  async function load() {
    failed = false;
    if (!inv) render();
    try {
      inv = await api.images({ signal });
    } catch (e) {
      if (isAbort(e)) return;
      if (!inv) failed = true; else showError(e);
    }
    render();
  }

  async function pull(ref) {
    if (state.get(ref)) return;
    state.set(ref, pulling(ref) ? '正在下载' : '正在准备下载');
    render();
    try {
      await pullImage(ref, { signal, onProgress: s => { state.set(ref, progressText(s)); render(); } });
      notify('已经下载好', { kind: 'ok' });
    } catch (e) {
      if (isAbort(e)) return;
      if (e instanceof ApiError) showError(e, { fallback: '下载没有成功' });
      else notify(e.message, { kind: 'bad' });
    }
    state.delete(ref);
    await load();
  }

  // ---- 从文件导入：上传校验 → 看清楚再确认 → 等导入完成 ----

  async function importFile(file) {
    const status = h('div', null, verdict('wait', { title: '正在校验文件', text: `${file.name}，${bytes(file.size)}。文件大的话要等一会儿。` }));
    let id = null;
    let loading = false;
    const cancel = btn('取消', { onClick: () => sheet && sheet.close('close') });
    const confirm = btn('导入', { cls: 'pri', disabled: true, onClick: () => doConfirm() });
    sheet = openSheet({
      title: '从文件导入客户端', sub: '先校验文件，确认以后才导入。', width: 'w520',
      body: status,
      foot: [h('span', { class: 'sp' }), cancel, confirm],
      onClose: () => {
        sheet = null;
        // 没开始导入的校验结果要丢掉；正在导入的不能丢，后台会接着做完
        if (id && !loading) api.imageImportDiscard(id).catch(() => {});
      },
    });
    const my = sheet;
    try {
      const r = await api.imageImport(file, { signal });
      if (sheet !== my) { if (r && r.id) api.imageImportDiscard(r.id).catch(() => {}); return; }
      id = r && r.id;
      fill(status, verdict('info', { title: '文件没有问题', text: previewText(r && r.preview) }));
      confirm.disabled = !id;
    } catch (e) {
      if (isAbort(e) || sheet !== my) return;
      const m = clean(e && e.reason);
      fill(status, verdict('bad', { title: '这个文件不能导入', text: m || '请换一个客户端离线包（.tar 或 .tar.gz 文件）。' }));
    }

    async function doConfirm() {
      if (!id || loading) return;
      loading = true;
      confirm.disabled = true;
      fill(status, verdict('wait', { title: '正在导入', text: '导入完成前可以关掉这个窗口，后台会接着做完。' }));
      try {
        let s = await api.imageImportConfirm(id, { signal });
        while (s && s.status === 'loading') {
          await new Promise(r => setTimeout(r, 1500));
          if (signal.aborted) return;
          s = await api.imageImportStatus(id, { signal });
        }
        if (s && s.status === 'done') {
          id = null;
          if (sheet === my) my.close('close');
          notify(clean(s.progress) || '已经导入', { kind: 'ok' });
          load();
          return;
        }
        throw new Error(clean(s && s.error) || '导入没有完成，刷新清单看看结果再决定要不要重来');
      } catch (e) {
        if (isAbort(e)) return;
        loading = false;
        // 请求本身没发成功（比如运行环境没连上）可以再点一次；后台已经报了导入失败的不能重来
        if (e instanceof ApiError) confirm.disabled = false;
        if (sheet === my) fill(status, verdict('bad', { title: '导入没有完成', text: clean(e && (e.reason || e.message)) || '刷新清单看看结果再决定要不要重来。' }));
        else showError(e);
      }
    }
  }

  load();
  return {
    update() {},
    unmount() { if (sheet) sheet.close('close'); },
  };
}
