// wyd web — runtime provenance. TUI-inspired:
//   Overview (categories) | Runtime (tree table) | Details
// One source of truth: `state`. Agent + human both go through dispatch().

const $ = (id) => document.getElementById(id);
const CSRF = document.querySelector('meta[name="csrf-token"]')?.getAttribute('content') || '';

const state = {
  mode: 'local',
  version: 0,
  sessions: [],
  items: [],        // nested tree
  overview: null,
  docker: null,
  query: '',
  selectedCategory: null,  // filter from Overview, or null = All
  section: 'runtime',      // runtime | ports | projects | docker | sessions
  project: null,           // project root-path filter for the runtime tree
  selection: null,         // selected item (flattened ref)
  proposal: null,
  confirmPid: null,
};

// ── SVG icons (inline, currentColor) ──
const SVG = {
  agent:  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="8" r="4"/><path d="M4 21v-2a8 8 0 0 1 16 0v2"/></svg>',
  mcp:    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3"/><path d="M19 9.3a7 7 0 0 1 0 5.4M5 9.3a7 7 0 0 0 0 5.4M12 3v3M12 18v3M3 12h3M18 12h3"/></svg>',
  browser:'<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="M3 12h18M12 3a15 15 0 0 1 0 18 15 15 0 0 1 0-18Z"/></svg>',
  db:     '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><ellipse cx="12" cy="5" rx="9" ry="3"/><path d="M3 5v14a9 3 0 0 0 18 0V5"/><path d="M3 12a9 3 0 0 0 18 0"/></svg>',
  dev:    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m8 8-4 4 4 4M16 8l4 4-4 4"/></svg>',
  other:  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="3" width="7" height="7" rx="1"/><rect x="14" y="3" width="7" height="7" rx="1"/><rect x="14" y="14" width="7" height="7" rx="1"/><rect x="3" y="14" width="7" height="7" rx="1"/></svg>',
  warn:   '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M10.29 3.86 1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0Z"/><line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/></svg>',
  ram:    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="7" width="20" height="11" rx="2"/><path d="M6 7V5M10 7V5M14 7V5M18 7V5"/></svg>',
  cpu:    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="5" y="5" width="14" height="14" rx="2"/><rect x="9" y="9" width="6" height="6"/><path d="M9 2v3M15 2v3M9 19v3M15 19v3M2 9h3M2 15h3M19 9h3M19 15h3"/></svg>',
  time:   '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/></svg>',
  pid:    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16M4 12h16M4 17h10"/></svg>',
  disk:   '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="10"/><circle cx="12" cy="12" r="2"/><path d="M12 2a10 10 0 0 1 10 10"/></svg>',
};
function icon(name, cls) {
  return `<span class="ico ${cls || ''}">${SVG[name] || ''}</span>`;
}

function fmtBytes(n) {
  if (!n) return '0 B';
  const u = ['B', 'KB', 'MB', 'GB'];
  let i = 0; let v = n;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(v >= 100 ? 0 : 1)} ${u[i]}`;
}
// Show the end of a path (the /Users/... prefix is the same for most).
function shortenPath(p, keep = 3) {
  if (!p) return '';
  const parts = p.replace(/^\/+/, '').split('/');
  if (parts.length <= keep) return p;
  return '…/' + parts.slice(-keep).join('/');
}
function fmtAge(secs) {
  if (!secs) return '—';
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h`;
  return `${Math.floor(secs / 86400)}d`;
}
function escapeHtml(s) {
  return String(s ?? '').replace(/[&<>"']/g, c => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
  }[c]));
}
function toast(msg) {
  const t = $('toast');
  t.textContent = msg;
  t.classList.add('show');
  clearTimeout(toast._t);
  toast._t = setTimeout(() => t.classList.remove('show'), 1800);
}

async function api(path, opts = {}) {
  const json = opts.json ? { ...opts.json, csrf: CSRF } : undefined;
  const r = await fetch(path, {
    headers: json ? { 'Content-Type': 'application/json' } : {},
    method: opts.method || 'GET',
    body: json ? JSON.stringify(json) : undefined,
  });
  const j = await r.json();
  if (!j.ok) throw new Error(j.error || 'request failed');
  return j.data;
}

async function poll() {
  try {
    const data = await api('/api/snapshot');
    state.mode = data.mode;
    state.version = data.version;
    state.items = data.items;
    state.overview = data.overview;
    state.sessions = data.sessions;
    state.docker = data.docker || null;
    render();
  } catch (e) { console.error('poll', e); }
}

// ── helpers ──

function flatten(items, out = []) {
  for (const i of items) { out.push(i); flatten(i.children || [], out); }
  return out;
}
function iconFor(item) {
  const cat = (item.category || '').toLowerCase();
  const what = (item.what || '');
  if (item.status === 'suspicious') return icon('warn', 'ico-suspicious');
  if (cat.includes('agent')) return icon('agent', 'ico-agent');
  if (cat.includes('mcp') || what === 'mcp') return icon('mcp', 'ico-mcp');
  if (cat.includes('browser') || what === 'browser') return icon('browser', 'ico-browser');
  if (cat.includes('database') || what === 'db') return icon('db', 'ico-db');
  if (cat.includes('dev') || what === 'dev' || what === 'ls') return icon('dev', 'ico-dev');
  return icon('other', 'ico-other');
}

function matchesQuery(it) {
  const q = state.query.trim().toLowerCase();
  if (!q) return true;
  const hay = [it.name, it.title, it.project, it.what, it.category,
    (it.reasons || []).join(' ')].filter(Boolean).join(' ').toLowerCase();
  return hay.includes(q);
}

function visibleItems() {
  let out = flatten(state.items);
  if (state.selectedCategory === 'suspicious') {
    out = out.filter(i => i.status === 'suspicious');
  } else if (state.selectedCategory) {
    out = out.filter(i => i.category === state.selectedCategory);
  }
  if (state.query) out = out.filter(matchesQuery);
  return out;
}

// ── reducer ──

function dispatch(action) {
  switch (action.type) {
    case 'select':
      state.selection = { kind: 'item', data: action.item };
      state.confirmPid = null;
      render();
      revealFocused();
      break;
    case 'focus-item': {
      const item = action.item;
      state.section = 'runtime';
      state.selectedCategory = item && item.status === 'suspicious' ? 'suspicious' : null;
      state.project = null;
      state.selection = { kind: 'item', data: item };
      state.confirmPid = null;
      render();
      revealFocused();
      break;
    }
    case 'focus-session': {
      state.section = 'sessions';
      state.selectedCategory = null;
      state.project = null;
      state.selection = { kind: 'session', data: action.session };
      state.confirmPid = null;
      render();
      revealFocused();
      break;
    }
    case 'show-leftovers':
      state.selectedCategory = 'suspicious';
      state.section = 'runtime';
      state.project = null;
      state.selection = null;
      render();
      break;
    case 'query':
      state.query = action.value;
      render();
      break;
    case 'terminate-ask':
      state.confirmPid = action.pid;
      render();
      break;
    case 'terminate-cancel':
      state.confirmPid = null;
      render();
      break;
    case 'terminate-go': {
      state.confirmPid = null;
      render();
      (async () => {
        try {
          const out = await api('/api/kill', { method: 'POST', json: { pid: action.pid } });
          toast(`Terminated PID ${action.pid} (${out.signaled} signaled${out.skipped ? ', ' + out.skipped + ' skipped' : ''})`);
          poll();
        } catch (e) { toast(`Failed: ${e.message}`); }
      })();
      break;
    }
    case 'terminate-force': {
      (async () => {
        try {
          const out = await api('/api/kill', { method: 'POST', json: { pid: action.pid, force: true } });
          toast(`Force-killed PID ${action.pid} (${out.signaled} signaled)`);
          poll();
        } catch (e) { toast(`Failed: ${e.message}`); }
      })();
      break;
    }
    case 'category': {
      state.selectedCategory = action.category;
      state.section = 'runtime';
      state.project = null;
      state.selection = null;
      render();
      break;
    }
    case 'section': {
      state.section = action.section;
      state.project = null;
      state.selection = null;
      render();
      break;
    }
    case 'project': {
      state.project = action.project;
      state.section = 'runtime';
      state.selectedCategory = null;
      state.selection = null;
      render();
      break;
    }
    case 'project-clear':
      state.project = null;
      render();
      break;
    case 'docker-stop': {
      (async () => {
        try {
          const out = await api('/api/docker/stop', { method: 'POST', json: { id: action.id } });
          toast(out.simulated ? 'Demo: stopped (simulated)' : `Stopped ${action.name}`);
          poll();
        } catch (e) { toast(`Failed: ${e.message}`); }
      })();
      break;
    }
    case 'docker-remove': {
      const persistent = action.persistent;
      const msg = persistent
        ? `Delete ${action.name}? PERSISTENT DATA — this cannot be undone.`
        : `Remove ${action.name}?`;
      if (!window.confirm(msg)) break;
      (async () => {
        try {
          const out = await api('/api/docker/remove', { method: 'POST', json: { id: action.id } });
          toast(out.simulated ? 'Demo: removed (simulated)' : `Removed ${action.name}`);
          poll();
        } catch (e) { toast(`Failed: ${e.message}`); }
      })();
      break;
    }
    case 'docker-prune': {
      if (!window.confirm('Prune all unused anonymous volumes? Named volumes and attached data are kept.')) break;
      (async () => {
        try {
          const out = await api('/api/docker/prune', { method: 'POST', json: {} });
          toast(out.simulated ? 'Demo: pruned (simulated)' : `Pruned ${out.pruned} volumes (${fmtBytes(out.reclaim_bytes)})`);
          poll();
        } catch (e) { toast(`Failed: ${e.message}`); }
      })();
      break;
    }
    case 'proposal':
      state.proposal = { ...action.proposal, id: action.id };
      render();
      revealProposal();
      break;
    case 'clear-proposal':
      state.proposal = null;
      render();
      break;
    case 'confirmed':
      toast(`Confirmed (${action.id}). Dry-run only.`);
      break;
    case 'refresh':
      poll();
      toast('Refreshed');
      break;
  }
}

// ── render ──

function render() {
  $('topbar-meta').textContent = `v${state.version} · ${state.mode}`;
  document.body.classList.toggle('demo', state.mode === 'demo');
  renderOverview();
  renderMain();
  renderProposal();
  const drawer = $('details-drawer');
  const has = !!state.selection;
  if (has) renderDetails();
  drawer.classList.toggle('open', has);
  drawer.hidden = !has;
  drawer.setAttribute('aria-hidden', has ? 'false' : 'true');
}

function renderOverview() {
  const list = $('overview-list');
  list.innerHTML = '';

  // A small metric segment: icon + value (e.g. [ram] 650 MB).
  const metric = (iconName, text, label) =>
    `<span class="ov-metric" title="${label}" aria-label="${label}">${SVG[iconName] || ''}<span class="ov-metric-v">${text}</span></span>`;

  // Two-line row: name + count on the first line, a small metric subtitle
  // (RAM / CPU icons with values) below.
  const ovRow = ({ cls = '', name, count, sub, onClick }) => {
    const row = document.createElement('div');
    row.className = 'ov-row' + (cls ? ' ' + cls : '');
    const subEl = sub ? `<div class="ov-sub">${sub}</div>` : '';
    row.innerHTML =
      `<div class="ov-line"><span class="ov-name">${name}</span><span class="ov-count">${count}</span></div>` +
      subEl;
    if (onClick) row.onclick = onClick;
    list.appendChild(row);
  };

  const on = (s, catOk = true) => state.section === s && catOk;

  // All row
  ovRow({
    cls: on('runtime', state.selectedCategory === null) ? 'selected' : '',
    name: 'All',
    count: state.overview?.total_items || 0,
    onClick: () => dispatch({ type: 'category', category: null }),
  });

  // Leftovers row (highlighted): subtitle = RAM metric icon + value.
  const loRam = state.overview?.leftover_memory_bytes
    ? metric('ram', escapeHtml(fmtBytes(state.overview.leftover_memory_bytes)), 'RAM')
    : '';
  ovRow({
    cls: 'suspicious' + (on('runtime', state.selectedCategory === 'suspicious') ? ' selected' : ''),
    name: 'Leftovers',
    count: state.overview?.suspicious || 0,
    sub: loRam,
    onClick: () => dispatch({ type: 'category', category: 'suspicious' }),
  });

  // Category rows: name = text, subtitle = RAM (+ CPU) metric icons.
  (state.overview?.categories || []).forEach(c => {
    const selected = on('runtime', state.selectedCategory === c.category);
    let sub = metric('ram', fmtBytes(c.memory_bytes), 'RAM');
    if (c.cpu_percent) sub += ' ' + metric('cpu', `${c.cpu_percent.toFixed(1)}%`, 'CPU');
    ovRow({
      cls: (selected ? 'selected' : '') + (c.count === 0 ? ' zero' : ''),
      name: escapeHtml(c.category),
      count: c.count,
      sub,
      onClick: () => dispatch({ type: 'category', category: c.category }),
    });
  });

  // Docker row: reclaimable disk belongs to Docker, never to Leftovers.
  const dk = state.docker;
  if (dk && dk.ok !== false) {
    const n = (dk.resources || []).length;
    let sub = metric('disk', fmtBytes(dk.disk_bytes), 'Disk');
    if (dk.reclaimable_bytes > 0) sub += ' ' + metric('disk', fmtBytes(dk.reclaimable_bytes), 'Reclaimable');
    ovRow({
      cls: on('docker') ? 'selected' : '',
      name: 'Docker',
      count: n,
      sub,
      onClick: () => dispatch({ type: 'section', section: 'docker' }),
    });
  }

  // Ports / Projects / Sessions rows.
  ovRow({
    cls: on('ports') ? 'selected' : '',
    name: 'Ports',
    count: state.overview?.ports || 0,
    onClick: () => dispatch({ type: 'section', section: 'ports' }),
  });
  ovRow({
    cls: on('projects') ? 'selected' : '',
    name: 'Projects',
    count: state.overview?.projects || 0,
    onClick: () => dispatch({ type: 'section', section: 'projects' }),
  });
  ovRow({
    cls: on('sessions') ? 'selected' : '',
    name: 'Sessions',
    count: state.sessions.length,
    onClick: () => dispatch({ type: 'section', section: 'sessions' }),
  });
}

// Build the display tree: reuse the backend tree, but pull orphan
// top-level resources (browsers/dev without children, not persistent) under
// the agent from the same project — matches the TUI's ancestry grouping.
function buildDisplayTree() {
  const tree = state.items.map(i => ({ ...i, children: (i.children || []).map(c => ({ ...c })) }));
  const orphans = tree.filter(i =>
    !i.children.length && i.project &&
    !['Agents'].includes(i.category) && i.category !== 'Databases'
  );
  const orphansByProject = new Map();
  for (const o of orphans) {
    if (!orphansByProject.has(o.project)) orphansByProject.set(o.project, []);
    orphansByProject.get(o.project).push(o);
  }
  const kept = tree.filter(i => !orphans.includes(i));
  const projectOf = (it) => it.project || '';
  for (const [proj, list] of orphansByProject) {
    const host = kept.find(i => i.category === 'Agents' && projectOf(i) === proj);
    if (host) {
      host.children = [...(host.children || []), ...list];
    } else {
      // no agent in this project: keep orphans top-level
      kept.push(...list);
    }
  }
  return kept;
}

function renderMain() {
  setPanel(state.section);
  switch (state.section) {
    case 'ports': renderPorts(); break;
    case 'projects': renderProjects(); break;
    case 'docker': renderDocker(); break;
    case 'sessions': renderSessions(); break;
    default: renderRuntime(); break;
  }
}

function setPanel(section) {
  const title = { runtime: 'Runtime', ports: 'Ports', projects: 'Projects', docker: 'Docker', sessions: 'Sessions' }[section] || 'Runtime';
  const head = document.querySelector('#panel-runtime .panel-head h2');
  if (head) head.textContent = title;
  const rtHead = document.querySelector('.runtime-table .rt-head');
  if (rtHead) rtHead.style.display = section === 'runtime' ? '' : 'none';
  const hint = $('runtime-hint');
  if (hint) hint.textContent = section === 'runtime' && state.project ? `project: ${state.project}` : '';
}

function allItems() {
  return flatten(state.items);
}

function filterQ(q, ...fields) {
  if (!q) return true;
  return fields.filter(Boolean).join(' ').toLowerCase().includes(q);
}

function basename(p) {
  if (!p) return '';
  return p.replace(/\/+$/, '').split('/').pop() || p;
}

function secRow(body, { name, nameTitle, what, from, fromTitle, meta, metaCls = '', action = '', onClick, cls = '' }) {
  const row = document.createElement('div');
  row.className = 'sec-row' + (onClick ? ' clickable' : '') + (cls ? ' ' + cls : '');
  row.innerHTML = `
    <span class="sec-name" title="${escapeHtml(nameTitle || '')}">${name}</span>
    <span class="sec-what">${what}</span>
    <span class="sec-from" title="${escapeHtml(fromTitle || '')}">${from}</span>
    <span class="sec-meta ${metaCls}">${meta}</span>
    <span class="sec-action">${action}</span>
  `;
  if (onClick) row.onclick = onClick;
  body.appendChild(row);
}

function renderPorts() {
  const body = $('runtime-rows');
  body.innerHTML = '';
  const q = state.query.trim().toLowerCase();
  const seen = new Set();
  let rows = [];
  for (const it of allItems()) {
    for (const p of it.ports || []) {
      if (seen.has(p.port)) continue;
      seen.add(p.port);
      rows.push({ port: p, owner: it });
    }
  }
  rows = rows.filter(r => filterQ(q, ':' + r.port.port, r.owner.title, r.owner.project));
  if (!rows.length) { body.innerHTML = `<div class="muted" style="padding:16px 8px">No ports.</div>`; return; }
  for (const r of rows) {
    const other = r.port.pid != null && Number(r.port.pid) && r.port.pid !== Number(r.owner.root_pid)
      ? ' <span class="sec-warn">(other)</span>' : '';
    secRow(body, {
      nameTitle: `${r.owner.title} :${r.port.port}`,
      name: `${iconFor(r.owner)}<span>${escapeHtml(String(r.port.address || ''))}:${escapeHtml(String(r.port.port))}</span>`,
      what: `<span class="sec-proto">${escapeHtml(r.port.protocol || 'tcp')}</span>`,
      fromTitle: r.owner.project || '',
      from: `${escapeHtml(r.owner.title)}${other}`,
      meta: `PID ${escapeHtml(String(r.port.pid ?? '—'))}`,
      onClick: () => dispatch({ type: 'select', item: r.owner }),
    });
  }
}

function renderProjects() {
  const body = $('runtime-rows');
  body.innerHTML = '';
  const q = state.query.trim().toLowerCase();
  const map = new Map();
  for (const it of allItems()) {
    if (!it.project) continue;
    let e = map.get(it.project);
    if (!e) { e = { path: it.project, name: basename(it.project), ram: 0, kids: 0 }; map.set(it.project, e); }
    e.ram += it.memory_bytes;
    e.kids += 1;
  }
  let rows = [...map.values()].filter(e => filterQ(q, e.name, e.path)).sort((a, b) => a.name.localeCompare(b.name));
  if (!rows.length) { body.innerHTML = `<div class="muted" style="padding:16px 8px">No projects.</div>`; return; }
  for (const e of rows) {
    secRow(body, {
      nameTitle: e.path,
      name: `<span>${escapeHtml(e.name)}</span>`,
      what: `<span class="sec-proto">project</span>`,
      fromTitle: e.path,
      from: escapeHtml(shortenPath(e.path)),
      meta: `${e.kids} item${e.kids === 1 ? '' : 's'} · ${fmtBytes(e.ram)}`,
      cls: state.project === e.path ? 'selected' : '',
      onClick: () => dispatch({ type: 'project', project: e.path }),
    });
  }
}

function renderDocker() {
  const body = $('runtime-rows');
  body.innerHTML = '';
  const dk = state.docker;
  if (!dk || dk.ok === false) {
    body.innerHTML = `<div class="muted" style="padding:16px 8px">Docker unavailable${dk && dk.note ? ' — ' + escapeHtml(dk.note) : ''}.</div>`;
    return;
  }
  const q = state.query.trim().toLowerCase();
  const res = (dk.resources || []).filter(r => filterQ(q, r.name, r.kind_label, r.detail, r.compose || ''));
  const prunable = (dk.resources || []).filter(r => r.anonymous && !r.persistent);
  if (prunable.length) {
    const bar = document.createElement('div');
    bar.className = 'sec-toolbar';
    const bytes = prunable.reduce((a, r) => a + r.size_bytes, 0);
    bar.innerHTML = `<span class="sec-toolbar-note">${prunable.length} anonymous volume${prunable.length === 1 ? '' : 's'} · ${fmtBytes(bytes)}</span><button class="btn-ghost-sm" id="docker-prune-btn">Prune</button>`;
    bar.querySelector('#docker-prune-btn').onclick = () => dispatch({ type: 'docker-prune' });
    body.appendChild(bar);
  }
  if (!res.length) { body.innerHTML += `<div class="muted" style="padding:16px 8px">No resources.</div>`; return; }
  for (const r of res) {
    const running = !!r.running;
    const marker = running ? '● ' : '○ ';
    const badge = r.persistent ? ' <span class="sec-warn">persistent</span>'
      : (r.anonymous ? ' <span class="sec-muted">anon</span>' : '');
    let action = '';
    if (running) action += `<button class="btn-ghost-sm" data-a="stop">Stop</button>`;
    action += `<button class="btn-ghost-sm" data-a="remove">${r.persistent ? 'Delete' : 'Remove'}</button>`;
    secRow(body, {
      nameTitle: `${r.name} (${r.id})`,
      name: `<span>${marker}${escapeHtml(r.name)}</span>`,
      what: `${escapeHtml(r.kind_label || '')}${badge}`,
      fromTitle: r.compose || '',
      from: escapeHtml(r.compose || '—'),
      meta: `${escapeHtml(r.detail || '')} · ${fmtBytes(r.size_bytes)}`,
      metaCls: running ? 'sec-ok' : 'sec-muted',
      action,
    });
    const last = body.lastElementChild;
    last.querySelector('[data-a="stop"]')?.addEventListener('click', (e) => {
      e.stopPropagation(); dispatch({ type: 'docker-stop', id: r.id, name: r.name });
    });
    last.querySelector('[data-a="remove"]')?.addEventListener('click', (e) => {
      e.stopPropagation(); dispatch({ type: 'docker-remove', id: r.id, name: r.name, persistent: r.persistent });
    });
  }
}

function renderSessions() {
  const body = $('runtime-rows');
  body.innerHTML = '';
  const q = state.query.trim().toLowerCase();
  const rows = state.sessions
    .filter(s => filterQ(q, s.agent, s.project, s.id))
    .sort((a, b) => (b.active - a.active) || (b.started_at - a.started_at));
  if (!rows.length) { body.innerHTML = `<div class="muted" style="padding:16px 8px">No sessions.</div>`; return; }
  for (const s of rows) {
    const selected = state.selection?.kind === 'session' && state.selection.data?.id === s.id;
    secRow(body, {
      nameTitle: `${s.agent} ${s.id}`,
      name: `<span>${escapeHtml(s.agent)}</span>`,
      what: `<span class="sec-proto">session</span>`,
      fromTitle: s.project || '',
      from: escapeHtml(shortenPath(s.project)) || '—',
      meta: `${s.active ? 'active' : 'ended'} · ${fmtAge(s.age_seconds)} · ${escapeHtml(String(s.id).slice(0, 8))}`,
      metaCls: s.active ? 'sec-ok' : 'sec-muted',
      cls: selected ? 'selected' : '',
      onClick: () => dispatch({ type: 'focus-session', session: s }),
    });
  }
}

function renderRuntime() {
  const body = $('runtime-rows');
  body.innerHTML = '';
  state.items = state.items; // keep raw
  const tree = buildDisplayTree();
  if (!tree.length) {
    body.innerHTML = `<div class="muted" style="padding:16px 8px">No items.</div>`;
    return;
  }

  const q = state.query.trim().toLowerCase();
  const project = state.project;
  const filtered = tree.filter(item => treeMatches(item, q, state.selectedCategory, project));
  if (!filtered.length) {
    body.innerHTML = `<div class="muted" style="padding:16px 8px">No items match.</div>`;
    return;
  }

  // When a category/status filter is active, hide the tree indentation
  // (children have no visible parent, so the tree connectors look wrong).
  const flat = !!state.selectedCategory || !!state.project;
  filtered.forEach(it => {
    renderTreeRow(body, it, 0, q, state.selectedCategory, flat, project);
  });
}

// does a subtree contain anything that matches the query/category/project?
function treeMatches(item, q, cat, project) {
  // A node matches if it satisfies the active category/status filter,
  // OR (if no category filter) it matches the text query.
  let self;
  if (cat === 'suspicious') {
    self = item.status === 'suspicious';
  } else if (cat) {
    self = item.category === cat;
  } else {
    self = !q || matchesQuery(item);
  }
  if (project && !projectMatch(item, project)) self = false;
  if (self) return true;
  return (item.children || []).some(c => treeMatches(c, q, cat, project));
}

function projectMatch(item, project) {
  return (item.project || '') === project;
}

// recursive tree row: parent + nested children with indent
function renderTreeRow(body, item, depth, q, cat, flat, project) {
  const children = (item.children || []).filter(c => treeMatches(c, q, cat, project));
  let show;
  if (cat === 'suspicious') {
    show = item.status === 'suspicious';
  } else if (cat) {
    show = item.category === cat;
  } else {
    show = !q || matchesQuery(item);
  }
  if (project && !projectMatch(item, project)) show = false;
  if (show) {
    body.appendChild(rtRow(item, depth, flat));
  }
  children.forEach(c => renderTreeRow(body, c, depth + 1, q, cat, flat, project));
}

function rtRow(it, depth, flat) {
  const row = document.createElement('div');
  const selected = state.selection?.data?.root_pid === it.root_pid && state.selection?.data?.name === it.name;
  row.className = 'rt-row' + (selected ? ' selected' : '');
  row.tabIndex = 0;
  const guide = flat || depth === 0 ? ''
    : '<span class="tree-guide" style="width:' + (depth * 8) + 'px"></span>' + '<span class="tree-guide">\u2514\u2500</span>';
  const fullName = it.title || it.name;
  row.innerHTML = `
    <span class="cell c-name" title="${escapeHtml(fullName)}">${guide}${iconFor(it)}<span>${escapeHtml(fullName)}</span></span>
    <span class="cell c-what" title="${escapeHtml(it.what || '')}">${escapeHtml(it.what || '')}</span>
    <span class="cell c-from" title="${escapeHtml(it.project || '')}">${escapeHtml(shortenPath(it.project))}</span>
    <span class="cell c-ram">${fmtBytes(it.memory_bytes)}</span>
    <span class="cell c-cpu">${(it.cpu_percent || 0).toFixed(1)}%</span>
    <span class="cell c-status"><span class="status ${it.status}" data-status="${it.status}" title="${it.status}">${it.status}</span></span>
    <span class="cell c-age">${fmtAge(it.age_seconds)}</span>
  `;
  row.onclick = () => dispatch({ type: 'select', item: it });
  row.onkeydown = (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); row.click(); } };
  return row;
}

function renderDetails() {
  const body = $('details-body');
  const sel = state.selection;

  // Proposal view: list + confirm (human keeps control)
  if (sel.kind === 'proposal') {
    const p = sel.data;
    const rows = (p.selected || []).map(s => `
      <div class="proposal-row selected"><span class="glyph">\u2713</span><span>${escapeHtml(s.title || s.name)}</span><span class="mem">${fmtBytes(s.memory_bytes)}</span></div>
    `).join('');
    const exc = (p.excluded || []).map(s => `
      <div class="proposal-row excluded"><span class="glyph">\u25CB</span><span>${escapeHtml(s.name)}${s.reason ? ' — ' + escapeHtml(s.reason) : ''}</span></div>
    `).join('');
    body.innerHTML = `
      <div class="verdict verdict-warn">
        <span class="verdict-label">Cleanup proposal</span>
        <span class="verdict-text">${(p.selected || []).length} items · ~${fmtBytes(p.reclaim_bytes)}</span>
      </div>
      ${rows}
      ${exc}
      <div class="evidence">
        <div class="evidence-label">Why these</div>
        <div>Everything selected is flagged as leftover (owning session ended, detached headless browser, long-running dev server). Persistent services like postgres/redis are excluded automatically.</div>
      </div>
      <div class="details-actions">
        <button class="btn-primary" id="drawer-confirm" ${(p.selected || []).length ? '' : 'disabled'}>Confirm cleanup</button>
      </div>
    `;
    const confirmBtn = document.getElementById('drawer-confirm');
    if (confirmBtn) {
      confirmBtn.onclick = async () => {
        try {
          const out = await api('/api/confirm', {
            method: 'POST',
            json: { id: state.proposal.id, version: state.proposal.snapshot_version },
          });
          toast(`Confirmed (${out.id}). Dry-run only.`);
          render();
        } catch (e) { toast(`Confirm failed: ${e.message}`); }
      };
    }
    return;
  }

  if (sel.kind === 'session') {
    const s = sel.data;
    const res = sessionResources(s);
    const leftover = res.filter(r => r.status === 'suspicious');
    body.innerHTML = `
      <div class="verdict ${s.active ? 'verdict-good' : 'verdict-warn'}">
        <span class="verdict-label">${s.active ? 'Active session' : 'Ended session'}</span>
        <span class="verdict-text">${escapeHtml(s.agent)}</span>
      </div>
      <div class="kv">
        <div class="k">Project</div><div class="v">${escapeHtml(s.project || '—')}</div>
        <div class="k">Id</div><div class="v">${escapeHtml(s.id)}</div>
        <div class="k">Age</div><div class="v">${fmtAge(s.age_seconds)}</div>
        <div class="k">Resources</div><div class="v">${res.length} · ${leftover.length} leftover</div>
      </div>
      ${res.length ? `
        <div class="evidence">
          <div class="evidence-label">Still running</div>
          <ul>${res.map(r => `<li>${escapeHtml(r.title || r.name)}${r.status === 'suspicious' ? ' · leftover' : ''}</li>`).join('')}</ul>
        </div>` : '<p class="hint">No runtime resources attributed to this session.</p>'}
    `;
    return;
  }

  const i = sel.data;
  const reasons = (i.reasons || []);
  const explanations = (i.explanations || []);
  const verdict = i.verdict
    ? i.verdict.charAt(0).toUpperCase() + i.verdict.slice(1)
    : (reasons.length
        ? `Leftover · ${reasons[0]}`
        : (i.status === 'persistent' ? 'Persistent service' : (i.status === 'active' ? 'Active' : 'Unknown')));
  const verdictTone = i.status === 'suspicious' ? 'warn'
    : i.status === 'persistent' ? 'neutral'
    : 'good';

  const evidence = (i.evidence || []);
  const evidenceBlock = evidence.length ? `
    <div class="evidence">
      <div class="evidence-label">Provenance</div>
      <ul>${evidence.map(e => `<li>${escapeHtml(e.kind)}${e.value ? ': ' + escapeHtml(e.value) : ''}</li>`).join('')}</ul>
    </div>
  ` : '';

  const owner = i.session;
  const ownerId = i.session_id || i.owner_session || owner?.id || '';
  const ownerEnded = owner && (owner.active === false || owner.ended_at);
  const ownerBlock = owner ? `
    <div class="evidence">
      <div class="evidence-label">Owning session</div>
      <ul>
        <li>${escapeHtml(owner.agent || 'unknown')}${ownerEnded ? ' · ended' : owner.active ? ' · active' : ''}${owner.project ? ' · ' + escapeHtml(owner.project) : ''}</li>
        ${ownerId ? `<li>id ${escapeHtml(String(ownerId))}</li>` : ''}
      </ul>
    </div>
  ` : '';

  // Observed listening sockets — NOT URLs. Each shows address, port,
  // protocol and owning PID; a socket owned by another PID is called out.
  const ports = i.ports || [];
  const listenerBlock = ports.length ? `
    <div class="listeners">
      <div class="listeners-label">Listening sockets <span class="l-count">${ports.length} TCP</span></div>
      ${ports.map(p => {
        const pid = p.pid != null ? Number(p.pid) : 0;
        const other = pid && pid !== Number(i.root_pid) ? ' <span class="l-other">(other)</span>' : '';
        const pidTxt = pid ? `PID ${pid}` : 'PID unknown';
        return `<div class="listener">
          <span class="l-addr">${escapeHtml(String(p.address || ''))}:${escapeHtml(String(p.port ?? ''))}</span>
          <span class="l-proto">${escapeHtml(p.protocol || 'tcp')}</span>
          <span class="l-pid">${pidTxt}${other}</span>
        </div>`;
      }).join('')}
    </div>
  ` : '';

  // Observed process identity.
  const cmd = i.cmd && i.cmd.length ? i.cmd.join(' ') : null;
  const identityBlock = (i.root_pid || i.ppid != null || i.cwd || cmd || i.tty) ? `
    <div class="evidence">
      <div class="evidence-label">Process</div>
      <div class="kv id-grid">
        ${i.root_pid ? `<div class="k">PID</div><div class="v">${i.root_pid}</div>` : ''}
        ${i.ppid != null ? `<div class="k">PPID</div><div class="v">${i.ppid}</div>` : ''}
        ${i.cwd ? `<div class="k">CWD</div><div class="v">${escapeHtml(i.cwd)}</div>` : ''}
        ${cmd ? `<div class="k">Command</div><div class="v cmd-wrap">${escapeHtml(cmd)}</div>` : ''}
        ${i.tty ? `<div class="k">TTY</div><div class="v">${escapeHtml(i.tty)}</div>` : ''}
      </div>
    </div>
  ` : '';

  body.innerHTML = `
    <div class="verdict verdict-${verdictTone}">
      <span class="verdict-label">Verdict</span>
      <span class="verdict-text">${escapeHtml(verdict)}</span>
    </div>
    <div class="summary">
      <span class="summary-item">${icon('ram')}${fmtBytes(i.memory_bytes)}</span>
      ${i.root_pid ? `<span class="summary-item">${icon('pid')}PID ${i.root_pid}</span>` : ''}
      ${(i.cpu_percent || 0) ? `<span class="summary-item">${icon('cpu')}${(i.cpu_percent || 0).toFixed(1)}%</span>` : ''}
      <span class="summary-item">${icon('time')}${fmtAge(i.age_seconds)}</span>
    </div>
    ${reasons.length ? `
      <div class="reason-block">
        <div class="reason-block-label">${i.score != null ? `Score ${i.score} / 100` : "Why it's flagged"}</div>
        <ul>${reasons.map((r, idx) => `
          <li>
            <span class="r-short">${escapeHtml(r)}</span>
            ${explanations[idx] ? `<div class="r-explain">${escapeHtml(explanations[idx])}</div>` : ''}
          </li>`).join('')}</ul>
      </div>
    ` : ''}
    ${evidenceBlock}
    ${ownerBlock}
    ${listenerBlock}
    ${identityBlock}
    <div class="kv">
      <div class="k">Category</div><div class="v">${escapeHtml(i.category)}</div>
      <div class="k">Status</div><div class="v">${escapeHtml(i.status)}</div>
      <div class="k">Project</div><div class="v">${escapeHtml(i.project || '—')}</div>
      <div class="k">Session</div><div class="v">${escapeHtml(String(ownerId).slice(0, 16)) || 'unattributed'}</div>
    </div>
    <div class="details-actions">
      <button class="btn-ask" id="copy-prompt">Copy investigation prompt</button>
      ${ports.length ? '<button class="btn-ask" id="open-http">Open as HTTP</button>' : ''}
      ${renderTerminate(i)}
    </div>
  `;

  const openBtn = document.getElementById('open-http');
  if (openBtn) {
    openBtn.onclick = () => {
      const p = ports[0];
      if (!p) return;
      const host = p.address && !/^0\.0\.0\.0$/.test(p.address) && !['::', '::0'].includes(p.address)
        ? p.address : '127.0.0.1';
      const url = `http://${host.includes(':') ? '[' + host + ']' : host}:${p.port}`;
      window.open(url, '_blank');
    };
  }

  const copyBtn = document.getElementById('copy-prompt');
  if (copyBtn) {
    copyBtn.onclick = async () => {
      const pid = i.root_pid;
      const name = i.title || i.name;
      if (!pid) { toast('No PID for this resource.'); return; }
      const promptText = `Explain why ${name} PID ${pid} is ${i.status} in wyd.`;
      try {
        await navigator.clipboard.writeText(promptText);
        toast('Prompt copied — paste it in your agent chat');
      } catch (e) {
        // clipboard API may be unavailable (http / older browsers): fallback
        try {
          const ta = document.createElement('textarea');
          ta.value = promptText;
          document.body.appendChild(ta);
          ta.select();
          document.execCommand('copy');
          document.body.removeChild(ta);
          toast('Prompt copied — paste it in your agent chat');
        } catch (e2) { toast('Copy failed: ' + e2.message); }
      }
    };
  }
}

function renderTerminate(i) {
  if (!i.root_pid) return '';
  if (state.confirmPid === i.root_pid) {
    return `
      <div class="confirm-inline">
        <span class="q">Stop ${escapeHtml(i.title || i.name)} (PID ${i.root_pid})?</span>
        <div class="confirm-btns">
          <button class="btn-confirm" id="confirm-yes">Yes, stop it</button>
          <button class="btn-cancel" id="confirm-no">Cancel</button>
        </div>
      </div>
    `;
  }
  return `
    <div class="term-btns">
      <button class="btn-terminate" id="terminate-btn">Terminate</button>
      <button class="btn-force" id="force-btn" title="SIGKILL — cannot be caught">Force kill</button>
    </div>
  `;
}

function renderProposal() {
  const summary = $('proposal-summary');
  const body = $('proposal-body');
  const actions = $('proposal-actions');
  const confirm = $('proposal-confirm');
  const dock = $('proposal');
  const p = state.proposal;
  const selected = (p && p.selected) || [];

  if (!p || !selected.length) {
    // empty: block stays visible but faded; nothing to confirm
    dock.classList.add('empty');
    body.innerHTML = '';
    summary.textContent = '';
    summary.classList.remove('proposal-summary-line');
    actions.hidden = true;
    confirm.disabled = true;
    return;
  }
  dock.classList.remove('empty');
  summary.textContent = `~${fmtBytes(p.reclaim_bytes)} · ${(p.excluded || []).length} excluded`;
  summary.classList.add('proposal-summary-line');
  const rows = selected.map(s => `
    <div class="proposal-row selected"><span class="glyph">✓</span><span>${escapeHtml(s.title || s.name)}</span><span class="mem">${fmtBytes(s.memory_bytes)}</span></div>
  `).join('');
  const exc = (p.excluded || []).map(s => `
    <div class="proposal-row excluded"><span class="glyph">○</span><span>${escapeHtml(s.name)}${s.reason ? ' — ' + escapeHtml(s.reason) : ''}</span></div>
  `).join('');
  body.innerHTML = rows + exc;
  actions.hidden = false;
  confirm.disabled = false;
}

// ── event wiring ──

document.getElementById('search').addEventListener('input', (e) => {
  dispatch({ type: 'query', value: e.target.value });
});

document.addEventListener('click', (e) => {
  const t = e.target;
  if (!t) return;
  if (t.id === 'terminate-btn') {
    const pid = Number(state.selection?.data?.root_pid);
    if (pid) dispatch({ type: 'terminate-ask', pid });
  } else if (t.id === 'force-btn') {
    const pid = Number(state.selection?.data?.root_pid);
    if (pid) dispatch({ type: 'terminate-force', pid });
  } else if (t.id === 'confirm-yes') {
    const pid = state.confirmPid;
    if (pid) dispatch({ type: 'terminate-go', pid });
  } else if (t.id === 'confirm-no') {
    dispatch({ type: 'terminate-cancel' });
  }
});

$('details-close').onclick = () => { state.selection = null; state.confirmPid = null; render(); };
document.getElementById('details-drawer').addEventListener('click', (e) => {
  if (e.target.id === 'details-drawer') {
    state.selection = null;
    state.confirmPid = null;
    render();
  }
});
$('proposal-confirm').onclick = async () => {
  if (!state.proposal) return;
  try {
    const out = await api('/api/confirm', {
      method: 'POST',
      json: { id: state.proposal.id, version: state.proposal.snapshot_version },
    });
    dispatch({ type: 'confirmed', id: out.id });
  } catch (e) { toast(`Confirm failed: ${e.message}`); }
};

document.querySelector('.brand').onclick = (e) => {
  e.preventDefault();
  dispatch({ type: 'refresh' });
};

document.addEventListener('keydown', (e) => {
  if (e.target.tagName === 'INPUT') return;
  if (e.key === 'Escape') {
    state.selection = null;
    state.confirmPid = null;
    render();
  } else if (e.key === 'r' && !e.metaKey && !e.ctrlKey) {
    dispatch({ type: 'refresh' });
  }
});

// ── theme toggle ──

function applyTheme(dark) {
  document.documentElement.setAttribute('data-theme', dark ? 'dark' : 'light');
  const t = document.getElementById('theme-toggle');
  if (t) t.textContent = dark ? '◐' : '◑';
  try { localStorage.setItem('wyd-theme', dark ? 'dark' : 'light'); } catch {}
}

document.getElementById('theme-toggle').addEventListener('click', () => {
  const dark = document.documentElement.getAttribute('data-theme') === 'dark';
  applyTheme(!dark);
});

// ── lookup helpers for agent tools ──

function findSession(id) {
  if (id == null || id === '') return null;
  const sid = String(id).toLowerCase();
  return state.sessions.find(s => String(s.id).toLowerCase() === sid)
    || state.sessions.find(s => String(s.id).toLowerCase().endsWith(sid))
    || null;
}

function findItem(id) {
  const items = flatten(state.items);
  const raw = String(id);
  const exact = items.find(i =>
    i.name === raw || i.title === raw || String(i.root_pid) === raw || String(i.session_id) === raw
  );
  if (exact) return exact;
  const q = raw.toLowerCase();
  const loose = items.filter(i =>
    (i.name || '').toLowerCase().includes(q) || (i.title || '').toLowerCase().includes(q)
  );
  if (loose.length === 1) return loose[0];
  return loose.find(i => i.status === 'suspicious') || loose[0] || null;
}

function collectPids(item, into = []) {
  if (item.root_pid != null) into.push(item.root_pid);
  for (const c of item.children || []) collectPids(c, into);
  return into;
}

function sessionResources(session) {
  if (!session) return [];
  const all = flatten(state.items);
  const bySid = all.filter(i => i.session_id && i.session_id === session.id);
  if (bySid.length) return bySid;
  const trees = state.items.filter(i =>
    (i.category || '').toLowerCase().includes('agent') &&
    (i.name || '').toLowerCase() === (session.agent || '').toLowerCase()
  );
  const pids = new Set();
  trees.forEach(t => collectPids(t).forEach(p => pids.add(p)));
  return all.filter(i => pids.has(i.root_pid));
}

function leftoverItems({ agent, project } = {}) {
  let out = flatten(state.items).filter(i => i.status === 'suspicious');
  if (agent) {
    const q = agent.toLowerCase();
    const sessionIds = new Set(
      state.sessions.filter(s => (s.agent || '').toLowerCase().includes(q)).map(s => s.id)
    );
    const pids = new Set();
    state.items
      .filter(i => (i.name || '').toLowerCase().includes(q))
      .forEach(t => collectPids(t).forEach(p => pids.add(p)));
    out = out.filter(i =>
      sessionIds.has(i.session_id) || pids.has(i.root_pid) ||
      (i.name || '').toLowerCase().includes(q)
    );
  }
  if (project) {
    const q = project.toLowerCase();
    out = out.filter(i => (i.project || '').toLowerCase().includes(q));
  }
  return out;
}

function revealFocused() {
  requestAnimationFrame(() => {
    const row = document.querySelector('.rt-row.selected, .sec-row.selected');
    if (!row) return;
    row.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
    row.classList.add('pulse');
    setTimeout(() => row.classList.remove('pulse'), 1400);
  });
}

function revealProposal() {
  requestAnimationFrame(() => {
    const dock = $('proposal');
    if (!dock || dock.classList.contains('empty')) return;
    dock.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
    dock.classList.add('pulse');
    setTimeout(() => dock.classList.remove('pulse'), 1400);
  });
}

// ── WebMCP tools ──

async function registerWebMcpTools() {
  const tools = [
    {
      name: 'list_sessions',
      title: 'List sessions',
      description: 'List coding-agent runtime sessions tracked by wyd. Filter by state (active or ended), agent name (e.g. opencode), or project. Ended sessions are the ones that leave leftovers running.',
      inputSchema: {
        type: 'object',
        properties: {
          state: { type: 'string', enum: ['active', 'ended'], description: 'Filter by session state' },
          agent: { type: 'string', description: 'Agent name substring, e.g. opencode' },
          project: { type: 'string', description: 'Project path substring' },
        },
      },
      annotations: { readOnlyHint: true },
      execute: ({ state: st, agent, project } = {}) => {
        let out = state.sessions.slice();
        if (st) out = out.filter(s => st === 'active' ? s.active : !s.active);
        if (agent) out = out.filter(s => s.agent.toLowerCase().includes(agent.toLowerCase()));
        if (project) out = out.filter(s => (s.project || '').toLowerCase().includes(project.toLowerCase()));
        return { sessions: out };
      },
    },
    {
      name: 'get_session',
      title: 'Get session',
      description: 'Get one session by id and the runtime resources it still owns. Focuses that session in the dashboard the human is looking at.',
      inputSchema: {
        type: 'object',
        properties: {
          session_id: { type: 'string', description: 'Session id from list_sessions' },
        },
        required: ['session_id'],
      },
      annotations: { readOnlyHint: true },
      execute: ({ session_id }) => {
        const s = findSession(session_id);
        if (!s) return { error: 'no such session' };
        const res = sessionResources(s);
        dispatch({ type: 'focus-session', session: s });
        return { session: s, resources: res };
      },
    },
    {
      name: 'list_leftovers',
      title: 'List leftovers',
      description: 'List leftover runtime resources (orphaned after an agent session ended), with reasons. Switches the dashboard to the Leftovers view. Filter by agent or project.',
      inputSchema: {
        type: 'object',
        properties: {
          agent: { type: 'string', description: 'Agent name substring, e.g. opencode' },
          project: { type: 'string', description: 'Project path substring' },
        },
      },
      annotations: { readOnlyHint: true },
      execute: ({ agent, project } = {}) => {
        dispatch({ type: 'show-leftovers' });
        const out = leftoverItems({ agent, project });
        return { leftovers: out, count: out.length };
      },
    },
    {
      name: 'explain_process',
      title: 'Explain process',
      description: 'Explain why wyd attributes a process to a session (equivalent to `wyd why`). Opens the details drawer with provenance evidence. Pass the process pid (e.g. Chromium 4102).',
      inputSchema: {
        type: 'object',
        properties: {
          pid: { type: 'number', description: 'Process pid to explain' },
        },
        required: ['pid'],
      },
      annotations: { readOnlyHint: true },
      execute: async ({ pid }) => {
        try {
          const data = await api(`/api/explain/${pid}`);
          const item = flatten(state.items).find(i => i.root_pid === pid) || findItem(String(pid));
          if (item) dispatch({ type: 'focus-item', item: { ...item, ...data } });
          return data;
        } catch (e) { return { error: e.message }; }
      },
    },
    {
      name: 'focus_resource',
      title: 'Focus resource',
      description: 'Highlight a session or resource in the dashboard the human is looking at. kind=session uses the session id; kind=item uses a resource name or pid.',
      inputSchema: {
        type: 'object',
        properties: {
          kind: { type: 'string', enum: ['session', 'item'], description: 'What to focus' },
          id: { type: 'string', description: 'Session id, resource name, or pid' },
        },
        required: ['kind', 'id'],
      },
      annotations: { readOnlyHint: true },
      execute: ({ kind, id }) => {
        if (kind === 'session') {
          const s = findSession(id);
          if (!s) return { ok: false, error: 'no such session' };
          dispatch({ type: 'focus-session', session: s });
          return { ok: true, focused: { kind, id: s.id, agent: s.agent } };
        }
        const item = findItem(id);
        if (!item) return { ok: false, error: 'no such resource' };
        dispatch({ type: 'focus-item', item });
        return { ok: true, focused: { kind, id: item.name, pid: item.root_pid } };
      },
    },
    {
      name: 'propose_cleanup',
      title: 'Propose cleanup',
      description: 'Build a cleanup proposal of leftover resources. Never kills anything. Fills the Cleanup proposal panel so the human can confirm. Persistent services (postgres, redis, mysql) are excluded.',
      inputSchema: {
        type: 'object',
        properties: {
          scope: { type: 'string', enum: ['leftovers', 'agent'], default: 'leftovers', description: 'What to propose cleaning' },
          agent: { type: 'string', description: 'When scope=agent, the agent name' },
        },
      },
      annotations: { readOnlyHint: true },
      execute: async ({ scope = 'leftovers', agent } = {}) => {
        const body = { scope, id: agent || '' };
        const out = await api('/api/proposal', { method: 'POST', json: body });
        state.selectedCategory = 'suspicious';
        state.section = 'runtime';
        state.project = null;
        dispatch({ type: 'proposal', id: out.id, proposal: out.proposal });
        return {
          id: out.id,
          selected: out.proposal.selected.length,
          excluded: out.proposal.excluded.length,
          reclaim_bytes: out.proposal.reclaim_bytes,
          snapshot_version: out.snapshot_version,
        };
      },
    },
  ];

  window.wydWebMcp = {
    names: tools.map(t => t.name),
    invoke(name, args = {}) {
      const t = tools.find(x => x.name === name);
      if (!t) return Promise.reject(new Error('unknown tool: ' + name));
      return Promise.resolve(t.execute(args));
    },
  };

  let registered = false;
  const tryReg = async () => {
    const reg = document.modelContext || navigator.modelContext;
    if (!reg || typeof reg.registerTool !== 'function') return false;
    if (registered) return true;
    registered = true;
    for (const t of tools) {
      try { await reg.registerTool(t); }
      catch (e) { console.warn('tool register failed', t.name, e); }
    }
    console.info(`wyd web: registered ${tools.length} WebMCP tools.`);
    return true;
  };

  if (await tryReg()) return;
  const started = Date.now();
  const iv = setInterval(async () => {
    if (await tryReg() || Date.now() - started > 15000) clearInterval(iv);
  }, 300);
}

// ── boot ──

(async function boot() {
  let saved = 'light';
  try { saved = localStorage.getItem('wyd-theme') || 'light'; } catch {}
  applyTheme(saved === 'dark');
  await poll();
  setInterval(poll, 2000);
  await registerWebMcpTools();
})();
