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
  query: '',
  selectedCategory: null,  // filter from Overview, or null = All
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
    case 'category':
      state.selectedCategory = action.category;
      state.selection = null;
      render();
      break;
    case 'select':
      state.selection = { kind: 'item', data: action.item };
      state.confirmPid = null;
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
    case 'proposal':
      state.proposal = { ...action.proposal, id: action.id };
      render();
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
  renderRuntime();
  renderProposal();
  // drawer follows selection state
  const drawer = $('details-drawer');
  if (state.selection) {
    renderDetails();
    drawer.classList.add('open');
  } else {
    drawer.classList.remove('open');
  }
}

function renderOverview() {
  const list = $('overview-list');
  list.innerHTML = '';

  // All row
  const all = document.createElement('div');
  all.className = 'ov-row' + (state.selectedCategory === null ? ' selected' : '');
  all.innerHTML = `<span class="ov-name">All</span><span class="ov-count">${state.overview?.total_items || 0}</span>`;
  all.onclick = () => dispatch({ type: 'category', category: null });
  list.appendChild(all);

  // Leftovers row (highlighted)
  const lo = document.createElement('div');
  lo.className = 'ov-row suspicious' + (state.selectedCategory === 'suspicious' ? ' selected' : '');
  lo.innerHTML = `<span class="ov-name">Leftovers</span><span class="ov-count">${state.overview?.suspicious || 0}</span>`;
  lo.onclick = () => dispatch({ type: 'category', category: 'suspicious' });
  list.appendChild(lo);

  // Category rows
  (state.overview?.categories || []).forEach(c => {
    const row = document.createElement('div');
    const selected = state.selectedCategory === c.category;
    row.className = 'ov-row' + (selected ? ' selected' : '') + (c.count === 0 ? ' zero' : '');
    row.innerHTML = `
      <span class="ov-name">${escapeHtml(c.category)}</span>
      <span class="ov-count">${c.count}</span>
      <span class="ov-mem">${fmtBytes(c.memory_bytes)}</span>
    `;
    row.onclick = () => dispatch({ type: 'category', category: c.category });
    list.appendChild(row);
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
  const filtered = tree.filter(item => treeMatches(item, q, state.selectedCategory));
  if (!filtered.length) {
    body.innerHTML = `<div class="muted" style="padding:16px 8px">No items match.</div>`;
    return;
  }

  // When a category/status filter is active, hide the tree indentation
  // (children have no visible parent, so the tree connectors look wrong).
  const flat = !!state.selectedCategory;
  filtered.forEach(it => {
    renderTreeRow(body, it, 0, q, state.selectedCategory, flat);
  });
}

// does a subtree contain anything that matches the query/category?
function treeMatches(item, q, cat) {
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
  if (self) return true;
  return (item.children || []).some(c => treeMatches(c, q, cat));
}

// recursive tree row: parent + nested children with indent
function renderTreeRow(body, item, depth, q, cat, flat) {
  const children = (item.children || []).filter(c => treeMatches(c, q, cat));
  let show;
  if (cat === 'suspicious') {
    show = item.status === 'suspicious';
  } else if (cat) {
    show = item.category === cat;
  } else {
    show = !q || matchesQuery(item);
  }
  if (show) {
    body.appendChild(rtRow(item, depth, flat));
  }
  children.forEach(c => renderTreeRow(body, c, depth + 1, q, cat, flat));
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

  const i = sel.data;
  const reasons = (i.reasons || []);
  const verdict = reasons.length
    ? `Leftover · ${reasons[0]}`
    : (i.status === 'persistent' ? 'Persistent service' : (i.status === 'active' ? 'Active' : 'Unknown'));
  const verdictTone = i.status === 'suspicious' ? 'warn'
    : i.status === 'persistent' ? 'neutral'
    : 'good';

  // evidence from explain_process if available
  const evidence = (i.evidence || []);
  const evidenceBlock = evidence.length ? `
    <div class="evidence">
      <div class="evidence-label">Provenance</div>
      <ul>${evidence.map(e => `<li>${escapeHtml(e.kind)}${e.value ? ': ' + escapeHtml(e.value) : ''}</li>`).join('')}</ul>
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
        <div class="reason-block-label">Why it's flagged</div>
        <ul>${reasons.map(r => `<li>${escapeHtml(r)}</li>`).join('')}</ul>
      </div>
    ` : ''}
    ${evidenceBlock}
    <div class="kv">
      <div class="k">Category</div><div class="v">${escapeHtml(i.category)}</div>
      <div class="k">Status</div><div class="v">${escapeHtml(i.status)}</div>
      <div class="k">Ports</div><div class="v">${(i.ports || []).map(p => p.port || p).join(', ') || '—'}</div>
      <div class="k">Project</div><div class="v">${escapeHtml(i.project || '—')}</div>
      <div class="k">Session</div><div class="v">${escapeHtml((i.session_id || '').slice(0, 16)) || 'unattributed'}</div>
    </div>
    <div class="details-actions">
      <button class="btn-ask" id="copy-prompt">Copy investigation prompt</button>
      ${renderTerminate(i)}
    </div>
  `;

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
  return `<button class="btn-terminate" id="terminate-btn">Terminate</button>`;
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

// ── WebMCP tools ──

async function registerWebMcpTools() {
  const reg = (navigator.modelContext || document.modelContext);
  if (!reg || typeof reg.registerTool !== 'function') return;

  const tools = [
    {
      name: 'list_sessions',
      description: 'List known coding-agent runtime sessions tracked by wyd.',
      inputSchema: { type: 'object', properties: { state: { type: 'string', enum: ['active', 'ended'] }, agent: { type: 'string' }, project: { type: 'string' } } },
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
      description: 'Return one session by id, with its resources.',
      inputSchema: { type: 'object', properties: { session_id: { type: 'string' } }, required: ['session_id'] },
      execute: ({ session_id }) => {
        const s = state.sessions.find(x => x.id === session_id);
        if (!s) return { error: 'no such session' };
        const res = flatten(state.items).filter(i => i.session_id === session_id);
        return { session: s, resources: res };
      },
    },
    {
      name: 'list_leftovers',
      description: 'List runtime resources considered leftover, with reasons.',
      inputSchema: { type: 'object', properties: { agent: { type: 'string' }, project: { type: 'string' } } },
      execute: ({ agent, project } = {}) => {
        let out = flatten(state.items).filter(i => i.status === 'suspicious');
        if (agent) out = out.filter(i => (i.category || '').toLowerCase().includes(agent.toLowerCase()));
        if (project) out = out.filter(i => (i.project || '').toLowerCase().includes(project.toLowerCase()));
        return { leftovers: out, count: out.length };
      },
    },
    {
      name: 'explain_process',
      description: 'Return ownership / provenance for a process by pid (equivalent to `wyd why`).',
      inputSchema: { type: 'object', properties: { pid: { type: 'number' } }, required: ['pid'] },
      execute: async ({ pid }) => {
        try {
          const data = await api(`/api/explain/${pid}`);
          const item = flatten(state.items).find(i => i.root_pid === pid);
          if (item) dispatch({ type: 'select', item: { ...item, ...data } });
          return data;
        } catch (e) { return { error: e.message }; }
      },
    },
    {
      name: 'focus_resource',
      description: 'Highlight and select a runtime resource or session in the visible UI.',
      inputSchema: { type: 'object', properties: { kind: { type: 'string', enum: ['session', 'item'] }, id: { type: 'string' } }, required: ['kind', 'id'] },
      execute: ({ kind, id }) => {
        if (kind === 'session') {
          const s = state.sessions.find(x => x.id === id);
          if (s) { state.selectedCategory = null; state.query = ''; }
        } else {
          const item = flatten(state.items).find(i => i.name === id || String(i.root_pid) === id);
          if (item) dispatch({ type: 'select', item });
        }
        render();
        return { ok: true };
      },
    },
    {
      name: 'propose_cleanup',
      description: 'Generate a cleanup proposal. Never performs side effects. Updates the visible proposal panel.',
      inputSchema: { type: 'object', properties: { scope: { type: 'string', enum: ['leftovers', 'agent'], default: 'leftovers' }, agent: { type: 'string' } } },
      execute: async ({ scope = 'leftovers', agent } = {}) => {
        const body = { scope, id: agent || '' };
        const out = await api('/api/proposal', { method: 'POST', json: body });
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

  for (const t of tools) {
    try { await reg.registerTool(t); }
    catch (e) { console.warn('tool register failed', t.name, e); }
  }
  console.info(`wyd web: registered ${tools.length} WebMCP tools.`);
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
