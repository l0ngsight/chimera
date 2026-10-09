'use strict';

// ---------------------------------------------------------------- utilities

const $app = document.getElementById('app');

function h(tag, attrs, ...children) {
  const el = tag === 'svg' || SVG_TAGS.has(tag)
    ? document.createElementNS('http://www.w3.org/2000/svg', tag)
    : document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v == null || v === false) continue;
    if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else if (k === 'class') el.setAttribute('class', v);
    else if (k === 'value') el.value = v;
    else if (k === 'checked') el.checked = !!v;
    else el.setAttribute(k, v === true ? '' : v);
  }
  for (const c of children.flat()) {
    if (c == null || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}
const SVG_TAGS = new Set(['g', 'rect', 'circle', 'line', 'path', 'text', 'defs', 'marker', 'polygon', 'title', 'tspan']);

async function api(method, path, body) {
  const res = await fetch(path, {
    method,
    headers: body !== undefined ? { 'Content-Type': 'application/json' } : {},
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  if (res.status === 204 || res.status === 202) return null;
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(data.error || `${res.status} ${res.statusText}`);
  return data;
}

function toast(msg, isError) {
  const t = h('div', { class: 'toast' + (isError ? ' error' : '') }, msg);
  document.getElementById('toasts').append(t);
  setTimeout(() => t.remove(), isError ? 7000 : 3500);
}

function fmtTime(ts) {
  const d = new Date(ts);
  const today = new Date().toDateString() === d.toDateString();
  return today ? d.toLocaleTimeString() : d.toLocaleString();
}

function slug(s) {
  return s.toLowerCase().replace(/[^a-z0-9]+/g, '_').replace(/^_+|_+$/g, '').slice(0, 40) || 'persona';
}

let cleanup = [];
function onLeave(fn) { cleanup.push(fn); }

let runtimesCache = null;
async function runtimes() {
  if (!runtimesCache) runtimesCache = await api('GET', '/api/runtimes');
  return runtimesCache;
}

// ---------------------------------------------------------------- router

const routes = [
  [/^#?\/?(?:\?.*)?$/, sessionsPage, 'sessions'],
  [/^#\/session\/(.+)$/, sessionPage, 'sessions'],
  [/^#\/teams$/, teamsPage, 'teams'],
  [/^#\/team\/(.+)$/, teamEditorPage, 'teams'],
  [/^#\/settings$/, settingsPage, 'settings'],
];

async function route() {
  cleanup.forEach(fn => { try { fn(); } catch (e) { console.error(e); } });
  cleanup = [];
  const hash = location.hash || '#/';
  for (const [re, page, nav] of routes) {
    const m = hash.match(re);
    if (!m) continue;
    document.querySelectorAll('[data-nav]').forEach(a => a.classList.toggle('active', a.dataset.nav === nav));
    $app.replaceChildren();
    try {
      await page(...m.slice(1).map(decodeURIComponent));
    } catch (e) {
      $app.replaceChildren(h('div', { class: 'page' }, h('div', { class: 'card' }, 'Error: ' + e.message)));
    }
    return;
  }
  location.hash = '#/';
}
window.addEventListener('hashchange', route);
window.addEventListener('beforeunload', e => { if (window.__dirty) { e.preventDefault(); e.returnValue = ''; } });

// ---------------------------------------------------------------- graph

const NODE_W = 184, NODE_H = 62;

/** Positions personas without saved coordinates in BFS layers from the entry. */
function layout(team, force) {
  const pos = {};
  const missing = team.personas.filter(p => force || !p.position);
  team.personas.forEach(p => { if (!force && p.position) pos[p.id] = { ...p.position }; });
  if (!missing.length) return pos;
  const adj = {};
  team.personas.forEach(p => adj[p.id] = new Set());
  team.edges.forEach(e => { adj[e.from]?.add(e.to); if (e.bidirectional) adj[e.to]?.add(e.from); });
  const depth = { [team.entry]: 0 };
  const queue = [team.entry];
  while (queue.length) {
    const id = queue.shift();
    for (const n of adj[id] || []) if (!(n in depth)) { depth[n] = depth[id] + 1; queue.push(n); }
  }
  const maxDepth = Math.max(0, ...Object.values(depth));
  const layers = {};
  missing.forEach(p => {
    const d = p.id in depth ? depth[p.id] : maxDepth + 1;
    (layers[d] = layers[d] || []).push(p.id);
  });
  for (const [d, ids] of Object.entries(layers)) {
    ids.forEach((id, i) => {
      pos[id] = { x: 120 + (i - (ids.length - 1) / 2) * 230 + 360, y: 80 + Number(d) * 150 };
    });
  }
  return pos;
}

/** Intersection of the segment from the node center towards (tx,ty) with the node box. */
function clip(cx, cy, tx, ty) {
  const dx = tx - cx, dy = ty - cy;
  if (!dx && !dy) return [cx, cy];
  const s = Math.min((NODE_W / 2 + 4) / Math.abs(dx || 1e-9), (NODE_H / 2 + 4) / Math.abs(dy || 1e-9));
  return [cx + dx * s, cy + dy * s];
}

/**
 * Renders a team graph into `wrap`. Returns a controller to update statuses etc.
 * opts: { editable, onSelect(id|null), onEdgeSelect(index), onMove(id,pos), onConnect(from,to), statusOf(id) }
 */
function createGraph(wrap, team, opts = {}) {
  const svg = h('svg', {});
  wrap.append(svg);
  let pos = layout(team);
  let selected = null, selectedEdge = null, connectFrom = null;
  let view = { x: 0, y: 0, w: 1000, h: 600 };
  const nodeEls = {}, edgeEls = [];

  function fit() {
    const pts = Object.values(pos);
    if (!pts.length) return;
    const xs = pts.map(p => p.x), ys = pts.map(p => p.y);
    const minX = Math.min(...xs) - NODE_W / 2 - 40, maxX = Math.max(...xs) + NODE_W / 2 + 40;
    const minY = Math.min(...ys) - NODE_H / 2 - 40, maxY = Math.max(...ys) + NODE_H / 2 + 40;
    const r = wrap.getBoundingClientRect();
    const aspect = r.width && r.height ? r.width / r.height : 1.6;
    let w = maxX - minX, hgt = maxY - minY;
    if (w / hgt < aspect) { const nw = hgt * aspect; view = { x: minX - (nw - w) / 2, y: minY, w: nw, h: hgt }; }
    else { const nh = w / aspect; view = { x: minX, y: minY - (nh - hgt) / 2, w, h: nh }; }
    svg.setAttribute('viewBox', `${view.x} ${view.y} ${view.w} ${view.h}`);
  }

  function toSvg(evt) {
    const pt = svg.createSVGPoint();
    pt.x = evt.clientX; pt.y = evt.clientY;
    return pt.matrixTransform(svg.getScreenCTM().inverse());
  }

  function render() {
    svg.replaceChildren();
    const defs = h('defs', {},
      ['arrow', 'arrow-hl'].map(id => h('marker', {
        id: id + '-' + uid, viewBox: '0 0 10 10', refX: 9, refY: 5, markerWidth: 7, markerHeight: 7, orient: 'auto-start-reverse',
      }, h('path', { d: 'M0,0 L10,5 L0,10 z', fill: id === 'arrow' ? '#5a6584' : '#7c9cff' }))));
    svg.append(defs);
    const edgesG = h('g', {}), nodesG = h('g', {});
    svg.append(edgesG, nodesG);
    edgeEls.length = 0;

    team.edges.forEach((e, i) => {
      const c = edgeCoords(e);
      if (!c) return;
      const [x1, y1, x2, y2] = c;
      const marker = `url(#arrow-${uid})`;
      const g = h('g', { class: 'edge' + (selectedEdge === i ? ' selected' : '') },
        h('line', { class: 'vis', x1, y1, x2, y2, 'marker-end': marker, 'marker-start': e.bidirectional ? marker : null }),
        h('line', { class: 'hit', x1, y1, x2, y2 }),
        h('title', {}, `${e.from} ${e.bidirectional ? '⇄' : '→'} ${e.to}`));
      if (opts.onEdgeSelect) g.addEventListener('click', ev => { ev.stopPropagation(); selectEdge(i); opts.onEdgeSelect(i); });
      g.__edge = e;
      g.dataset.from = e.from; g.dataset.to = e.to; g.dataset.bi = e.bidirectional ? '1' : '';
      edgesG.append(g);
      edgeEls.push(g);
    });

    for (const p of team.personas) {
      const at = pos[p.id];
      const status = opts.statusOf ? opts.statusOf(p.id) : null;
      const sub = [p.runtime, p.model, p.effort].filter(Boolean).join(' · ');
      const g = h('g', {
        class: ['node', status, selected === p.id && 'selected', connectFrom === p.id && 'connect-src'].filter(Boolean).join(' '),
        transform: `translate(${at.x - NODE_W / 2},${at.y - NODE_H / 2})`,
      },
        h('rect', { class: 'box', width: NODE_W, height: NODE_H }),
        h('rect', { class: 'stripe', x: 0, y: 8, width: 4, height: NODE_H - 16, fill: runtimeColor(p.runtime) }),
        h('text', { class: 'title', x: 14, y: 22 }, ellipsize(p.name, 22)),
        h('text', { class: 'sub', x: 14, y: 39 }, ellipsize(sub, 28)),
        status ? h('circle', { class: 'st', cx: 20, cy: 51, r: 4 }) : null,
        status ? h('text', { class: 'status', x: 29, y: 54.5 }, status) : null,
        p.id === team.entry ? h('text', { class: 'entry-mark', x: NODE_W - 10, y: 16, 'text-anchor': 'end' }, 'ENTRY') : null,
        h('title', {}, `${p.name}\n${p.role || ''}\nid: ${p.id}`));
      nodeEls[p.id] = g;
      attachNodeEvents(g, p.id);
      nodesG.append(g);
    }
  }

  function attachNodeEvents(g, id) {
    let drag = null;
    g.addEventListener('pointerdown', ev => {
      ev.stopPropagation();
      if (!opts.editable || connectFrom !== null || opts.connectMode?.()) return;
      const p = toSvg(ev);
      drag = { dx: p.x - pos[id].x, dy: p.y - pos[id].y, moved: false };
      g.setPointerCapture(ev.pointerId);
    });
    g.addEventListener('pointermove', ev => {
      if (!drag) return;
      const p = toSvg(ev);
      const nx = Math.round(p.x - drag.dx), ny = Math.round(p.y - drag.dy);
      if (Math.abs(nx - pos[id].x) + Math.abs(ny - pos[id].y) > 2) drag.moved = true;
      pos[id] = { x: nx, y: ny };
      g.setAttribute('transform', `translate(${nx - NODE_W / 2},${ny - NODE_H / 2})`);
      redrawEdges();
    });
    g.addEventListener('pointerup', ev => {
      const wasDrag = drag && drag.moved;
      drag = null;
      if (wasDrag) { opts.onMove?.(id, pos[id]); return; }
      ev.stopPropagation();
      if (opts.connectMode?.()) {
        if (connectFrom === null) { connectFrom = id; render(); }
        else { const from = connectFrom; connectFrom = null; if (from !== id) opts.onConnect?.(from, id); render(); }
        return;
      }
      select(id);
      opts.onSelect?.(id);
    });
  }

  /** Endpoints of an edge, clipped to the node boxes; antiparallel one-way edges are offset. */
  function edgeCoords(e) {
    const a = pos[e.from], b = pos[e.to];
    if (!a || !b) return null;
    const reverse = !e.bidirectional && team.edges.some(o => o.from === e.to && o.to === e.from && !o.bidirectional);
    let off = [0, 0];
    if (reverse) {
      const len = Math.hypot(b.x - a.x, b.y - a.y) || 1;
      off = [-(b.y - a.y) / len * 7, (b.x - a.x) / len * 7];
    }
    return [
      ...clip(a.x + off[0], a.y + off[1], b.x + off[0], b.y + off[1]),
      ...clip(b.x + off[0], b.y + off[1], a.x + off[0], a.y + off[1]),
    ];
  }

  // Moves edge lines in place while dragging (re-rendering would break pointer capture).
  function redrawEdges() {
    edgeEls.forEach(g => {
      const c = edgeCoords(g.__edge);
      if (!c) return;
      g.querySelectorAll('line').forEach(l => {
        l.setAttribute('x1', c[0]); l.setAttribute('y1', c[1]); l.setAttribute('x2', c[2]); l.setAttribute('y2', c[3]);
      });
    });
  }

  function select(id) { selected = id; selectedEdge = null; render(); }
  function selectEdge(i) { selectedEdge = i; selected = null; render(); }

  svg.addEventListener('pointerup', ev => {
    if (ev.target === svg) {
      if (connectFrom !== null) { connectFrom = null; render(); return; }
      selected = null; selectedEdge = null; render();
      opts.onSelect?.(null);
    }
  });

  const uid = Math.random().toString(36).slice(2, 8);
  render();
  requestAnimationFrame(fit);
  const ro = new ResizeObserver(() => fit());
  ro.observe(wrap);

  return {
    render,
    fit,
    select,
    setTeam(t, relayout) { team = t; pos = relayout ? layout(t, true) : { ...layout(t), ...pos }; render(); },
    positions: () => pos,
    setStatus(id, status) {
      const g = nodeEls[id];
      if (!g) return;
      g.classList.remove('idle', 'queued', 'running', 'waiting', 'error', 'done');
      g.classList.add(status);
      const t = g.querySelector('text.status');
      if (t) t.textContent = status;
    },
    flash(from, to) {
      const g = edgeEls.find(e => (e.dataset.from === from && e.dataset.to === to) || (e.dataset.bi && e.dataset.from === to && e.dataset.to === from));
      if (!g) return;
      g.classList.remove('flash'); void g.getBBox(); g.classList.add('flash');
      setTimeout(() => g.classList.remove('flash'), 1700);
      // A dot travelling along the edge in the message direction.
      const a = pos[from], b = pos[to];
      if (!a || !b) return;
      const dot = h('circle', { class: 'edge-pulse', r: 5, cx: a.x, cy: a.y });
      svg.append(dot);
      const start = performance.now();
      const step = now => {
        const t = Math.min(1, (now - start) / 900);
        dot.setAttribute('cx', a.x + (b.x - a.x) * t);
        dot.setAttribute('cy', a.y + (b.y - a.y) * t);
        if (t < 1) requestAnimationFrame(step); else dot.remove();
      };
      requestAnimationFrame(step);
    },
    destroy() { ro.disconnect(); },
    clearConnect() { connectFrom = null; render(); },
  };
}

function ellipsize(s, n) { s = s || ''; return s.length > n ? s.slice(0, n - 1) + '…' : s; }

function runtimeColor(rt) {
  return { claude: '#d97757', codex: '#10a37f', opencode: '#a78bfa', hermes: '#f5c542', mock: '#64748b' }[rt] || '#7c9cff';
}

// ---------------------------------------------------------------- sessions list

async function sessionsPage() {
  const [teams, sessions] = await Promise.all([api('GET', '/api/teams'), api('GET', '/api/sessions')]);
  const preselect = new URLSearchParams(location.hash.split('?')[1] || '').get('team');

  const teamSel = h('select', {}, teams.map(t => h('option', { value: t.id }, t.name)));
  if (preselect) teamSel.value = preselect;
  const task = h('textarea', { rows: 6, placeholder: 'Describe the task for the team…' });
  const title = h('input', { placeholder: 'optional, defaults to the first line of the task' });
  const workdir = h('input', { placeholder: 'optional; default: a fresh directory under data/workspaces' });
  const dry = h('input', { type: 'checkbox' });
  const startBtn = h('button', { class: 'primary', onclick: start }, 'Start session');

  async function start() {
    if (!task.value.trim()) return toast('Enter a task first', true);
    startBtn.disabled = true;
    try {
      const s = await api('POST', '/api/sessions', {
        team_id: teamSel.value, task: task.value, title: title.value || null, workdir: workdir.value || null, dry_run: dry.checked,
      });
      location.hash = '#/session/' + s.id;
    } catch (e) { toast(e.message, true); startBtn.disabled = false; }
  }

  const listBody = h('tbody');
  function renderList(list) {
    listBody.replaceChildren(...(list.length ? list.map(s => h('tr', { class: 'clickable', onclick: () => location.hash = '#/session/' + s.id },
      h('td', {}, h('div', {}, s.title), h('div', { class: 'muted small' }, s.id)),
      h('td', {}, s.team_name, s.dry_run ? h('span', { class: 'badge plain small', style: 'margin-left:6px' }, 'dry run') : null),
      h('td', {}, h('span', { class: 'badge ' + s.status }, s.status)),
      h('td', {}, h('span', { class: 'dots' }, Object.entries(s.agents).map(([id, a]) => h('span', { class: 'dot ' + a.status, title: `${id}: ${a.status}` })))),
      h('td', { class: 'muted' }, s.total_turns),
      h('td', { class: 'muted small' }, fmtTime(s.created_at)),
    )) : [h('tr', {}, h('td', { colspan: 6, class: 'empty' }, 'No sessions yet. Start one on the left.'))]));
  }
  renderList(sessions);
  const timer = setInterval(async () => { try { renderList(await api('GET', '/api/sessions')); } catch { } }, 2500);
  onLeave(() => clearInterval(timer));

  $app.append(h('div', { class: 'page' },
    h('div', { class: 'grid two' },
      h('div', { class: 'card' },
        h('h2', {}, 'New session'),
        teams.length ? null : h('p', { class: 'muted' }, 'No teams yet — ', h('a', { href: '#/team/new' }, 'create one'), '.'),
        h('label', {}, 'Team'), teamSel,
        h('label', {}, 'Task'), task,
        h('label', {}, 'Title'), title,
        h('label', {}, 'Working directory'), workdir,
        h('div', { class: 'row', style: 'margin-top:12px' },
          h('label', { class: 'inline', title: 'Every persona uses the built-in mock runtime: no CLI or LLM calls. Good for checking the graph.' }, dry, 'Dry run (mock agents)'),
          h('span', { class: 'spacer' }), startBtn)),
      h('div', { class: 'card' },
        h('h2', {}, 'Sessions'),
        h('table', {}, h('thead', {}, h('tr', {}, ['Session', 'Team', 'Status', 'Agents', 'Turns', 'Started'].map(c => h('th', {}, c)))), listBody)))));
}

// ---------------------------------------------------------------- session view

async function sessionPage(id) {
  const [data, settings] = await Promise.all([api('GET', '/api/sessions/' + id), api('GET', '/api/settings').catch(() => ({}))]);
  const team = data.team;
  const jevRouting = settings.decisions_active && settings.decisions.route_user_messages && !data.summary.dry_run;
  let summary = data.summary;
  const events = [];
  let lastSeq = 0;
  const agentStatus = Object.fromEntries(Object.entries(summary.agents).map(([k, a]) => [k, a.status]));
  let selected = null;
  let tab = 'timeline';
  const personaName = pid => pid === 'user' ? 'You' : (team.personas.find(p => p.id === pid)?.name || pid);

  // Header
  const statusBadge = h('span', { class: 'badge ' + summary.status }, summary.status);
  const statusDetail = h('span', { class: 'muted small' }, summary.status_detail || '');
  const cancelBtn = h('button', { class: 'danger', onclick: async () => { try { await api('POST', `/api/sessions/${id}/cancel`); } catch (e) { toast(e.message, true); } } }, 'Cancel');
  const deleteBtn = h('button', {
    class: 'danger', onclick: async () => {
      if (!confirm('Delete this session and its log? (The workspace directory is kept.)')) return;
      try { await api('DELETE', '/api/sessions/' + id); location.hash = '#/'; } catch (e) { toast(e.message, true); }
    },
  }, 'Delete');
  const turnsEl = h('span', { class: 'muted small' });

  function updateHeader() {
    statusBadge.className = 'badge ' + summary.status;
    statusBadge.textContent = summary.status;
    statusDetail.textContent = summary.status_detail || '';
    cancelBtn.classList.toggle('hidden', summary.status !== 'running');
    deleteBtn.classList.toggle('hidden', summary.status === 'running');
    turnsEl.textContent = `${summary.total_turns} turns`;
  }

  // Graph
  const graphWrap = h('div', { class: 'graph-wrap' },
    h('div', { class: 'legend' }, ['idle', 'queued', 'running', 'waiting', 'done', 'error'].map(s => h('span', {}, h('span', { class: 'dot ' + s }), s))),
    h('div', { class: 'graph-hint' }, 'Click a persona to see its log'));
  const graph = createGraph(graphWrap, team, {
    statusOf: pid => agentStatus[pid] || 'idle',
    onSelect: pid => { selected = pid; tab = pid ? 'log' : 'timeline'; renderPanel(); },
  });
  onLeave(() => graph.destroy());

  // Right panel
  const panelTitle = h('span', { class: 'name' });
  const panelSub = h('span', { class: 'muted small' });
  const tabs = h('div', { class: 'tabs' });
  const body = h('div', { class: 'panel-body' });
  const toSel = h('select', { style: 'width:auto' },
    jevRouting ? h('option', { value: 'auto', title: 'Jev picks the persona best suited to answer' }, '⚡ Auto (Jev routes)') : null,
    team.personas.map(p => h('option', { value: p.id }, p.name + (p.id === team.entry ? ' (entry)' : ''))));
  toSel.value = jevRouting ? 'auto' : team.entry;
  const msgInput = h('textarea', { placeholder: 'Send a message or follow-up instruction…' });
  const sendBtn = h('button', { class: 'primary', onclick: send }, 'Send');
  msgInput.addEventListener('keydown', e => { if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) send(); });

  async function send() {
    if (!msgInput.value.trim()) return;
    sendBtn.disabled = true;
    try { await api('POST', `/api/sessions/${id}/messages`, { to: toSel.value, content: msgInput.value }); msgInput.value = ''; }
    catch (e) { toast(e.message, true); }
    sendBtn.disabled = false;
  }

  function renderPanel() {
    const p = selected && team.personas.find(x => x.id === selected);
    if (p) {
      panelTitle.textContent = p.name;
      panelSub.textContent = `${p.role || ''} · ${p.runtime}${p.model ? ' / ' + p.model : ''} · ${p.effort}`;
      toSel.value = p.id;
      tabs.replaceChildren(
        h('button', { class: tab === 'log' ? 'active' : '', onclick: () => { tab = 'log'; renderPanel(); } }, 'Log'),
        h('button', { class: tab === 'persona' ? 'active' : '', onclick: () => { tab = 'persona'; renderPanel(); } }, 'Persona'),
        h('button', { onclick: () => { selected = null; tab = 'timeline'; graph.select(null); renderPanel(); } }, '× Close'));
    } else {
      panelTitle.textContent = 'Timeline';
      panelSub.textContent = 'messages between personas';
      tabs.replaceChildren();
    }
    body.replaceChildren();
    if (p && tab === 'persona') {
      body.append(
        h('div', { class: 'small muted' }, 'id: ', h('code', {}, p.id)),
        h('h3', { style: 'margin-top:12px' }, 'Prompt'),
        h('pre', { class: 'log-entry', style: 'white-space:pre-wrap' }, p.prompt || '(none)'),
        h('h3', {}, 'Can message'),
        h('div', {}, recipients(team, p.id).map(r => h('span', { class: 'badge plain', style: 'margin:2px' }, r)) || '—'));
      return;
    }
    const relevant = events.filter(e => matches(e));
    if (!relevant.length) body.append(h('div', { class: 'empty' }, p ? 'No activity yet.' : 'No messages yet.'));
    relevant.forEach(e => { const el = renderEvent(e); if (el) body.append(el); });
    body.scrollTop = body.scrollHeight;
  }

  function matches(e) {
    if (!selected) return ['message', 'final', 'session_status', 'decision'].includes(e.type);
    if (e.type === 'message') return e.from === selected || e.to === selected;
    return e.agent === selected && e.type !== 'agent_status';
  }

  function renderEvent(e) {
    switch (e.type) {
      case 'message': {
        const toMe = selected && e.to === selected;
        return h('div', { class: 'msg' + (e.rejected ? ' rejected' : '') + (e.from === 'user' ? ' from-user' : '') },
          h('div', { class: 'msg-head' },
            selected ? (toMe ? '← from ' : '→ to ') : null,
            selected ? h('b', {}, personaName(toMe ? e.from : e.to)) : [h('b', {}, personaName(e.from)), ' → ', h('b', {}, personaName(e.to))],
            e.rejected ? h('span', { style: 'color:var(--error)' }, '(not delivered: no channel)') : null,
            e.deferred ? h('span', { class: 'jev', title: 'Jev judged this an acknowledgement: delivered without starting a turn; read on the next one' }, '⚡ deferred') : null,
            h('span', { class: 'spacer' }), fmtTime(e.ts)),
          h('div', { class: 'msg-body' }, e.content));
      }
      case 'final':
        return h('div', { class: 'msg final' }, h('div', { class: 'msg-head' }, '✔ Final answer from ', h('b', {}, personaName(e.agent))), h('div', { class: 'msg-body' }, e.content));
      case 'session_status':
        return h('div', { class: 'status-line' }, `session ${e.status}${e.detail ? ' — ' + e.detail : ''} · ${fmtTime(e.ts)}`);
      case 'turn_start':
        return h('div', { class: 'turn-sep' }, `turn #${e.turn} · ${fmtTime(e.ts)}`);
      case 'turn_end':
        return h('div', { class: 'status-line' }, `turn #${e.turn} finished in ${(e.duration_ms / 1000).toFixed(1)}s` + (e.error ? ` with error` : ''));
      case 'decision': return renderDecision(e);
      case 'log': return renderLog(e);
      default: return null;
    }
  }

  function renderDecision(e) {
    const label = { ack_filter: 'ack filter', resolve_recipient: 'recipient', detect_final: 'completion', route_user: 'routing' }[e.kind] || e.kind;
    const outcome = e.kind === 'route_user' || e.kind === 'resolve_recipient' ? personaName(e.outcome) : e.outcome;
    const effect = !e.applied ? '' : {
      ack_filter: ' → delivered without waking',
      resolve_recipient: ' → message re-addressed',
      detect_final: ' → session completed',
      route_user: ' → message sent there',
    }[e.kind] || '';
    return h('div', { class: 'decision' + (e.error ? ' failed' : e.applied ? ' applied' : '') },
      h('span', { class: 'jev' }, '⚡ Jev'), ' ', h('b', {}, label), ' · ', e.subject, ': ',
      e.error ? h('span', { title: e.error }, 'skipped (' + e.error.slice(0, 120) + ')') : [
        h('b', {}, outcome),
        e.confidence != null ? h('span', { class: 'muted' }, ` (${Math.round(e.confidence * 100)}% conf, ${e.latency_ms}ms${e.cost != null ? ', $' + e.cost.toFixed(5) : ''})`) : null,
        effect,
      ]);
  }

  function renderLog(e) {
    const long = e.text.length > 600 || e.text.split('\n').length > 12;
    if (e.kind === 'prompt') return h('div', { class: 'log-entry' }, h('details', {}, h('summary', {}, 'prompt sent to the agent'), h('pre', {}, e.text)));
    if ((e.kind === 'tool_result' || e.kind === 'thinking' || e.kind === 'stderr') && long) {
      return h('div', { class: 'log-entry ' + e.kind }, h('details', {}, h('summary', {}, e.text.split('\n')[0].slice(0, 120) + ' …'), h('pre', {}, e.text)));
    }
    return h('div', { class: 'log-entry ' + e.kind }, e.text);
  }

  function apply(e) {
    if (e.seq <= lastSeq) return;
    lastSeq = e.seq;
    events.push(e);
    let rerender = false;
    if (e.type === 'agent_status') { agentStatus[e.agent] = e.status; graph.setStatus(e.agent, e.status); }
    if (e.type === 'session_status') { summary.status = e.status; summary.status_detail = e.detail; updateHeader(); }
    if (e.type === 'turn_start') { summary.total_turns = Math.max(summary.total_turns, e.turn); updateHeader(); }
    if (e.type === 'message' && live && !e.rejected) graph.flash(e.from, e.to);
    if (live && matches(e) && tab !== 'persona') {
      if (body.querySelector('.empty')) rerender = true;
      else {
        const nearBottom = body.scrollHeight - body.scrollTop - body.clientHeight < 80;
        const el = renderEvent(e);
        if (el) body.append(el);
        if (nearBottom) body.scrollTop = body.scrollHeight;
      }
    }
    if (rerender) renderPanel();
  }

  updateHeader();
  $app.append(h('div', { class: 'page full' },
    h('div', { class: 'session-head' },
      h('a', { href: '#/' }, '← Sessions'),
      h('h1', {}, summary.title), statusBadge, statusDetail,
      h('span', { class: 'spacer' }),
      h('span', { class: 'badge plain' }, team.name), summary.dry_run ? h('span', { class: 'badge plain' }, 'dry run') : null,
      turnsEl, cancelBtn, deleteBtn),
    h('div', { class: 'session-body' },
      h('div', { class: 'session-left' },
        graphWrap,
        h('div', { class: 'card' },
          h('div', { class: 'row between' }, h('h3', { style: 'margin:0' }, 'Task'), h('span', { class: 'muted small mono', title: 'shared working directory' }, summary.workdir)),
          h('div', { class: 'task-box', style: 'margin-top:8px' }, summary.task))),
      h('div', { class: 'panel' },
        h('div', { class: 'panel-head' }, panelTitle, panelSub, h('span', { class: 'spacer' }), tabs),
        body,
        h('div', { class: 'composer' },
          h('div', { class: 'row' }, h('span', { class: 'muted small' }, 'To'), toSel, h('span', { class: 'spacer' }), h('span', { class: 'muted small' }, 'Ctrl+Enter to send'), sendBtn),
          msgInput)))));

  // Live stream: replays history first, then live events.
  let live = false;
  const es = new EventSource(`/api/sessions/${id}/stream`);
  es.onmessage = m => apply(JSON.parse(m.data));
  onLeave(() => es.close());
  // The replay arrives in one burst; render the panel once afterwards.
  setTimeout(() => { live = true; renderPanel(); }, 400);
  renderPanel();
}

function recipients(team, pid) {
  return team.personas.filter(p => p.id !== pid && team.edges.some(e => (e.from === pid && e.to === p.id) || (e.bidirectional && e.to === pid && e.from === p.id))).map(p => p.name);
}

// ---------------------------------------------------------------- teams list

async function teamsPage() {
  const teams = await api('GET', '/api/teams');
  async function duplicate(t) {
    const copy = structuredClone(t);
    let n = 2;
    while (teams.some(x => x.id === `${t.id}-${n}`)) n++;
    copy.id = `${t.id}-${n}`;
    copy.name = `${t.name} (copy)`;
    try { await api('POST', '/api/teams', copy); location.hash = '#/team/' + copy.id; } catch (e) { toast(e.message, true); }
  }
  async function remove(t) {
    if (!confirm(`Delete team "${t.name}"?`)) return;
    try { await api('DELETE', '/api/teams/' + t.id); route(); } catch (e) { toast(e.message, true); }
  }
  $app.append(h('div', { class: 'page' },
    h('div', { class: 'row between', style: 'margin-bottom:16px' }, h('h1', { style: 'margin:0' }, 'Teams'), h('a', { class: 'btn', href: '#/team/new' }, '+ New team')),
    h('div', { class: 'grid cards' }, teams.map(t => h('div', { class: 'card' },
      h('div', { class: 'row between' }, h('h2', { style: 'margin:0' }, t.name), h('span', { class: 'muted small' }, t.id)),
      h('p', { class: 'muted' }, t.description || ''),
      h('div', { class: 'small' }, t.personas.map(p => h('div', { class: 'row', style: 'gap:6px;margin:3px 0' },
        h('span', { class: 'dot', style: `background:${runtimeColor(p.runtime)}` }),
        h('b', {}, p.name), h('span', { class: 'muted' }, `${p.runtime}${p.model ? ' / ' + p.model : ''} · ${p.effort}`),
        p.id === t.entry ? h('span', { class: 'badge plain small' }, 'entry') : null))),
      h('div', { class: 'row', style: 'margin-top:14px' },
        h('a', { class: 'btn', href: '#/team/' + t.id }, 'Edit'),
        h('a', { class: 'btn', href: '#/?team=' + t.id }, 'Start session'),
        h('button', { onclick: () => duplicate(t) }, 'Duplicate'),
        h('span', { class: 'spacer' }),
        h('button', { class: 'danger', onclick: () => remove(t) }, 'Delete')))),
      teams.length ? null : h('div', { class: 'empty' }, 'No teams yet.'))));
}

// ---------------------------------------------------------------- team editor

const EFFORTS = ['minimal', 'low', 'medium', 'high', 'xhigh', 'max', 'ultra'];
const MAX_MODEL_ROWS = 100;

/**
 * Model combobox fed by the runtime's own model list (asked from its CLI or API by the server).
 *
 * Deliberately not a native <select>: CLIs such as opencode can list thousands of models, and a
 * native popup that long is drawn by the browser process itself (shared by every window) and can
 * hang or crash it. This list lives in the page, filters as you type and renders at most
 * MAX_MODEL_ROWS rows. Any typed text is accepted as a custom model id.
 */
function modelPicker(p, onChange) {
  const input = h('input', { value: p.model, placeholder: 'runtime default — type to search', autocomplete: 'off', spellcheck: 'false' });
  const menu = h('div', { class: 'combo-menu hidden', role: 'listbox' });
  const info = h('div', { class: 'small muted', style: 'margin-top:4px' });
  let list = null, loadSeq = 0, autoPick = false, rows = [], active = -1;
  const models = () => list?.models || [];
  const current = () => models().find(m => m.id === p.model);

  async function load(refresh) {
    const seq = ++loadSeq, runtime = p.runtime;
    info.replaceChildren(h('span', { class: 'spinner' }), ' asking the CLI for its models…');
    let res;
    try { res = await api('GET', `/api/runtimes/${encodeURIComponent(runtime)}/models${refresh ? '?refresh=true' : ''}`); }
    catch (e) { res = { models: [], error: e.message }; }
    if (seq !== loadSeq) return; // a newer runtime was picked meanwhile
    list = res;
    if (autoPick && !p.model && models().length) { p.model = models()[0].id; input.value = p.model; onChange(); }
    autoPick = false;
    renderInfo();
    if (!menu.classList.contains('hidden')) renderMenu();
  }

  function commit(id) {
    id = id.trim();
    input.value = id;
    close();
    if (id === p.model) return;
    p.model = id;
    renderInfo();
    onChange();
  }

  function open() { menu.classList.remove('hidden'); renderMenu(); }
  function close() { menu.classList.add('hidden'); active = -1; }

  function renderMenu() {
    const q = input.value.trim().toLowerCase();
    // Show everything while the field still holds the current model; filter once the user types.
    const filter = q && q !== (p.model || '').toLowerCase();
    const matches = filter
      ? models().filter(m => [m.id, m.name, m.group].some(v => v && v.toLowerCase().includes(q)))
      : models();
    rows = matches.slice(0, MAX_MODEL_ROWS);
    const items = [];
    let group;
    rows.forEach((m, i) => {
      if ((m.group || '') !== group) {
        group = m.group || '';
        if (group) items.push(h('div', { class: 'combo-group' }, group));
      }
      items.push(h('div', {
        class: 'combo-item' + (m.id === p.model ? ' selected' : '') + (i === active ? ' active' : ''),
        role: 'option',
        onmousedown: e => { e.preventDefault(); commit(m.id); },
      },
        h('span', {}, m.name && m.name !== m.id ? m.name : m.id),
        m.name && m.name !== m.id ? h('span', { class: 'muted mono small' }, ' ' + m.id) : null,
        m.context ? h('span', { class: 'muted small' }, ` · ${Math.round(m.context / 1000)}k`) : null));
    });
    if (!matches.length) {
      items.push(h('div', { class: 'combo-note' }, models().length
        ? `No listed model matches — press Enter to use “${input.value.trim()}” as a custom id.`
        : 'No model list available — type a model id.'));
    } else if (matches.length > rows.length) {
      items.push(h('div', { class: 'combo-note' }, `${matches.length - rows.length} more — keep typing to narrow down.`));
    }
    menu.replaceChildren(...items);
    menu.querySelector('.active')?.scrollIntoView({ block: 'nearest' });
  }

  function renderInfo() {
    const parts = [];
    if (list?.error) parts.push(h('span', { class: 'avail no', title: list.error }, '⚠ ' + list.error.slice(0, 160) + ' — showing suggestions. '));
    else if (list?.source === 'cli') parts.push(`${models().length} models from `, h('code', {}, list.via), '. ');
    else if (list?.source === 'api') parts.push(`${models().length} models (aliases + ${list.via}). `);
    else if (list?.note) parts.push(list.note + ' ');
    if (p.model && list && !current()) parts.push(h('span', {}, 'Custom id (not in the list). '));
    parts.push(h('a', { href: 'javascript:void 0', onclick: () => load(true) }, '↻ refresh'));
    const m = current();
    if (m?.description) parts.push(h('div', {}, m.description));
    info.replaceChildren(...parts);
  }

  input.addEventListener('focus', () => { input.select(); open(); });
  input.addEventListener('input', () => { active = -1; open(); });
  input.addEventListener('blur', () => commit(input.value));
  input.addEventListener('keydown', e => {
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      if (menu.classList.contains('hidden')) open();
      if (!rows.length) return;
      active = (active + (e.key === 'ArrowDown' ? 1 : -1) + rows.length) % rows.length;
      renderMenu();
    } else if (e.key === 'Enter') {
      e.preventDefault();
      commit(active >= 0 && rows[active] ? rows[active].id : input.value);
      input.blur();
    } else if (e.key === 'Escape') {
      input.value = p.model;
      close();
      input.blur();
    }
  });

  return {
    el: h('div', { class: 'combo' }, input, menu, info),
    load,
    current,
    /** Called when the runtime changes: forget the old model and fetch the new list. */
    reset() { p.model = ''; input.value = ''; list = null; autoPick = true; renderInfo(); load(false); },
  };
}

async function teamEditorPage(teamId) {
  const isNew = teamId === 'new';
  const rts = await runtimes();
  let team = isNew
    ? { id: '', name: '', description: '', entry: 'lead', max_turns: 60, max_parallel: 4, edges: [], personas: [newPersona('lead', 'Lead', rts)] }
    : await api('GET', '/api/teams/' + teamId);
  let selection = null; // {kind:'persona', id} | {kind:'edge', index}
  let connectMode = false;
  let bidiDefault = true;
  const dirtyEl = h('span', { class: 'dirty' });
  const setDirty = d => { window.__dirty = d; dirtyEl.textContent = d ? '● unsaved changes' : ''; };
  setDirty(isNew);
  onLeave(() => setDirty(false));

  // Team metadata form
  const f = {
    name: h('input', { value: team.name, oninput: e => { team.name = e.target.value; if (isNew && !idTouched) { team.id = slug(team.name).replace(/_/g, '-'); f.id.value = team.id; } setDirty(true); } }),
    id: h('input', { value: team.id, disabled: !isNew, oninput: e => { idTouched = true; team.id = e.target.value; setDirty(true); } }),
    entry: h('select', { onchange: e => { team.entry = e.target.value; setDirty(true); graph.render(); } }),
    max_turns: h('input', { type: 'number', min: 1, value: team.max_turns, oninput: e => { team.max_turns = +e.target.value; setDirty(true); } }),
    max_parallel: h('input', { type: 'number', min: 1, value: team.max_parallel, oninput: e => { team.max_parallel = +e.target.value; setDirty(true); } }),
    description: h('textarea', { rows: 2, value: team.description, oninput: e => { team.description = e.target.value; setDirty(true); } }),
  };
  let idTouched = false;
  function refreshEntry() {
    f.entry.replaceChildren(...team.personas.map(p => h('option', { value: p.id }, p.name)));
    f.entry.value = team.entry;
  }
  refreshEntry();

  const connectBtn = h('button', { onclick: () => { connectMode = !connectMode; connectBtn.classList.toggle('active', connectMode); graph.clearConnect(); hint.textContent = connectMode ? 'Click the sender, then the receiver.' : defaultHint; } }, '⤳ Connect');
  const bidiBox = h('input', { type: 'checkbox', checked: bidiDefault, onchange: e => bidiDefault = e.target.checked });
  const defaultHint = 'Drag to move · click a persona or an arrow to edit it';
  const hint = h('div', { class: 'graph-hint' }, defaultHint);
  const graphWrap = h('div', { class: 'graph-wrap' }, hint);
  const side = h('div', { class: 'card editor-side' });

  const graph = createGraph(graphWrap, team, {
    editable: true,
    connectMode: () => connectMode,
    onSelect: id => { selection = id ? { kind: 'persona', id } : null; renderSide(); },
    onEdgeSelect: i => { selection = { kind: 'edge', index: i }; renderSide(); },
    onMove: (id, p) => { team.personas.find(x => x.id === id).position = p; setDirty(true); },
    onConnect: (from, to) => {
      if (team.edges.some(e => (e.from === from && e.to === to) || (e.bidirectional && e.from === to && e.to === from))) return toast('That channel already exists');
      const rev = team.edges.find(e => e.from === to && e.to === from);
      if (rev && bidiDefault) rev.bidirectional = true;
      else team.edges.push({ from, to, bidirectional: bidiDefault });
      setDirty(true);
      graph.setTeam(team);
    },
  });
  onLeave(() => graph.destroy());

  function addPersona() {
    let n = team.personas.length + 1, id = 'persona_' + n;
    while (team.personas.some(p => p.id === id)) id = 'persona_' + (++n);
    const p = newPersona(id, 'New persona ' + n, rts);
    const pos = graph.positions();
    const xs = Object.values(pos);
    p.position = { x: xs.length ? Math.max(...xs.map(q => q.x)) + 230 : 400, y: xs.length ? xs[0].y : 100 };
    team.personas.push(p);
    setDirty(true);
    refreshEntry();
    graph.setTeam(team);
    graph.fit();
    selection = { kind: 'persona', id };
    graph.select(id);
    renderSide();
  }

  function relayout() {
    team.personas.forEach(p => delete p.position);
    graph.setTeam(team, true);
    const pos = graph.positions();
    team.personas.forEach(p => p.position = pos[p.id]);
    graph.fit();
    setDirty(true);
  }

  async function save() {
    const pos = graph.positions();
    team.personas.forEach(p => { if (pos[p.id]) p.position = { x: Math.round(pos[p.id].x), y: Math.round(pos[p.id].y) }; });
    try {
      if (isNew) await api('POST', '/api/teams', team);
      else await api('PUT', '/api/teams/' + team.id, team);
      setDirty(false);
      toast('Team saved');
      if (isNew) location.hash = '#/team/' + team.id;
    } catch (e) { toast(e.message, true); }
  }

  function renderSide() {
    side.replaceChildren();
    if (selection?.kind === 'persona') {
      const p = team.personas.find(x => x.id === selection.id);
      if (p) return side.append(personaForm(p));
    }
    if (selection?.kind === 'edge') {
      const e = team.edges[selection.index];
      if (e) return side.append(edgeForm(e, selection.index));
    }
    side.append(
      h('h2', {}, 'Communication channels'),
      h('p', { class: 'muted small' }, 'Arrows show who may send messages to whom. Use “Connect” to add one; click an arrow to change or remove it.'),
      team.edges.length ? h('table', {}, h('tbody', {}, team.edges.map((e, i) => h('tr', { class: 'clickable', onclick: () => { selection = { kind: 'edge', index: i }; renderSide(); } },
        h('td', {}, nameOf(e.from)), h('td', { class: 'muted' }, e.bidirectional ? '⇄' : '→'), h('td', {}, nameOf(e.to)))))) : h('div', { class: 'empty' }, 'No channels yet.'),
      h('h2', { style: 'margin-top:20px' }, 'Personas'),
      h('div', {}, team.personas.map(p => h('div', { class: 'row', style: 'margin:4px 0' },
        h('span', { class: 'dot', style: `background:${runtimeColor(p.runtime)}` }),
        h('a', { href: 'javascript:void 0', onclick: () => { selection = { kind: 'persona', id: p.id }; graph.select(p.id); renderSide(); } }, p.name),
        h('span', { class: 'muted small' }, `${p.runtime} · ${p.model || 'default model'} · ${p.effort}`)))));
  }

  const nameOf = id => team.personas.find(p => p.id === id)?.name || id;

  function edgeForm(e, i) {
    return h('div', {},
      h('div', { class: 'row between' }, h('h2', { style: 'margin:0' }, 'Channel'), h('button', { onclick: () => { selection = null; renderSide(); graph.select(null); } }, '×')),
      h('p', {}, h('b', {}, nameOf(e.from)), e.bidirectional ? ' ⇄ ' : ' → ', h('b', {}, nameOf(e.to))),
      h('label', { class: 'inline' }, h('input', { type: 'checkbox', checked: e.bidirectional, onchange: ev => { e.bidirectional = ev.target.checked; setDirty(true); graph.render(); renderSide(); } }), 'Bidirectional (both can message each other)'),
      h('div', { class: 'row', style: 'margin-top:14px' },
        h('button', { onclick: () => { [e.from, e.to] = [e.to, e.from]; setDirty(true); graph.render(); renderSide(); } }, '⇆ Reverse'),
        h('span', { class: 'spacer' }),
        h('button', { class: 'danger', onclick: () => { team.edges.splice(i, 1); selection = null; setDirty(true); graph.setTeam(team); renderSide(); } }, 'Delete channel')));
  }

  function personaForm(p) {
    const changed = (rerenderGraph = true) => { setDirty(true); if (rerenderGraph) graph.render(); };
    const rtSel = h('select', {
      onchange: e => { p.runtime = e.target.value; picker.reset(); changed(); renderEfforts(); },
    }, rts.map(r => h('option', { value: r.name }, `${r.label || r.name}${r.available ? '' : ' (not installed)'}`)));
    rtSel.value = p.runtime;
    // The options of this native <select> are built once and never touched again: rewriting a
    // native select's options while its popup may be open (e.g. when the model list finishes
    // loading) is a known way to crash Chrome's browser process on macOS.
    const effortSel = h('select', { onchange: e => { p.effort = e.target.value; changed(); renderEfforts(); } },
      EFFORTS.map(x => h('option', { value: x }, x)));
    effortSel.value = p.effort;
    const effortNote = h('div', { class: 'small avail no' });
    // Warn when the chosen model does not support the effort (after the runtime's effort mapping).
    const renderEfforts = () => {
      const supported = picker.current()?.efforts;
      const map = rts.find(r => r.name === p.runtime)?.effort_map || {};
      const ok = e => !supported?.length || map[e] === '' || supported.includes(map[e] ?? e);
      effortNote.textContent = ok(p.effort) ? '' : `${p.model} does not support “${p.effort}” effort (supports: ${supported.join(', ')}).`;
    };
    const picker = modelPicker(p, () => { changed(); renderEfforts(); });
    picker.load(false).then(renderEfforts);
    renderEfforts();
    const idIn = h('input', { value: p.id });
    idIn.addEventListener('change', () => {
      const nid = slug(idIn.value);
      if (nid === p.id) return;
      if (team.personas.some(x => x.id === nid)) { toast(`id "${nid}" is taken`, true); idIn.value = p.id; return; }
      team.edges.forEach(e => { if (e.from === p.id) e.from = nid; if (e.to === p.id) e.to = nid; });
      if (team.entry === p.id) team.entry = nid;
      const pos = graph.positions(); pos[nid] = pos[p.id];
      p.id = nid; idIn.value = nid; selection.id = nid;
      refreshEntry(); changed(); graph.setTeam(team);
    });
    const promptTa = h('textarea', { class: 'code', rows: 14, value: p.prompt, oninput: e => { p.prompt = e.target.value; changed(false); } });
    const desc = h('textarea', { rows: 3, placeholder: 'e.g. Senior Rust engineer focused on async networking; pragmatic, writes thorough tests.' });
    const genBtn = h('button', { class: 'primary', onclick: () => generate(false) }, '✨ Generate prompt');
    const refineBtn = h('button', { onclick: () => generate(true) }, 'Refine current');
    const genStatus = h('span', { class: 'muted small' });

    async function generate(refine) {
      genBtn.disabled = refineBtn.disabled = true;
      genStatus.replaceChildren(h('span', { class: 'spinner' }), ' generating…');
      try {
        const res = await api('POST', '/api/personas/generate', {
          name: p.name, role: p.role, description: desc.value, runtime: p.runtime, model: p.model,
          team_name: team.name, team_description: team.description,
          teammates: team.personas.filter(x => x.id !== p.id).map(x => `${x.name} — ${x.role}`),
          current_prompt: refine ? p.prompt : '',
        });
        p.prompt = res.prompt; promptTa.value = res.prompt; changed(false);
        genStatus.textContent = 'done';
      } catch (e) { genStatus.textContent = ''; toast(e.message, true); }
      genBtn.disabled = refineBtn.disabled = false;
    }

    const extra = h('input', { class: 'mono', value: (p.extra_args || []).join(' '), placeholder: 'e.g. --add-dir ../shared', oninput: e => { p.extra_args = e.target.value.split(/\s+/).filter(Boolean); changed(false); } });
    const env = h('textarea', {
      class: 'code', rows: 2, placeholder: 'KEY=value per line',
      value: Object.entries(p.env || {}).map(([k, v]) => `${k}=${v}`).join('\n'),
      oninput: e => {
        p.env = Object.fromEntries(e.target.value.split('\n').map(l => l.trim()).filter(l => l.includes('=')).map(l => [l.slice(0, l.indexOf('=')).trim(), l.slice(l.indexOf('=') + 1)]));
        changed(false);
      },
    });
    const rt = rts.find(r => r.name === p.runtime);

    return h('div', {},
      h('div', { class: 'row between' }, h('h2', { style: 'margin:0' }, 'Persona'), h('button', { onclick: () => { selection = null; graph.select(null); renderSide(); } }, '×')),
      h('div', { class: 'grid', style: 'grid-template-columns:1fr 1fr;gap:0 10px' },
        h('div', {}, h('label', {}, 'Name'), h('input', { value: p.name, oninput: e => { p.name = e.target.value; changed(); refreshEntry(); } })),
        h('div', {}, h('label', {}, 'Id (used in messages)'), idIn)),
      h('label', {}, 'Role'), h('input', { value: p.role, placeholder: 'e.g. Senior Backend Engineer', oninput: e => { p.role = e.target.value; changed(false); } }),
      h('div', { class: 'grid', style: 'grid-template-columns:1fr 150px;gap:0 10px' },
        h('div', {}, h('label', {}, 'Runtime'), rtSel),
        h('div', {}, h('label', {}, 'Effort'), effortSel)),
      effortNote,
      h('label', {}, 'Model'), picker.el,
      rt && !rt.available ? h('div', { class: 'avail no' }, `“${rt.command}” was not found on PATH of the server.`) : null,
      h('div', { class: 'gen-box' },
        h('div', { class: 'small muted' }, 'Describe the persona and let the configured LLM write its prompt (see Settings).'),
        desc,
        h('div', { class: 'row', style: 'margin-top:6px' }, genBtn, refineBtn, genStatus)),
      h('label', {}, 'System prompt'), promptTa,
      h('details', { style: 'margin-top:10px' }, h('summary', { class: 'muted small' }, 'Advanced'),
        h('label', {}, 'Extra CLI arguments'), extra,
        h('label', {}, 'Environment variables'), env),
      h('div', { class: 'row', style: 'margin-top:14px' },
        team.entry === p.id ? h('span', { class: 'badge plain' }, 'entry persona') : h('button', { onclick: () => { team.entry = p.id; refreshEntry(); changed(); renderSide(); } }, 'Make entry'),
        h('span', { class: 'spacer' }),
        h('button', {
          class: 'danger', onclick: () => {
            if (team.personas.length === 1) return toast('A team needs at least one persona', true);
            if (!confirm(`Remove ${p.name}?`)) return;
            team.personas = team.personas.filter(x => x !== p);
            team.edges = team.edges.filter(e => e.from !== p.id && e.to !== p.id);
            if (team.entry === p.id) team.entry = team.personas[0].id;
            selection = null; refreshEntry(); setDirty(true); graph.setTeam(team); renderSide();
          },
        }, 'Remove persona')));
  }

  renderSide();
  $app.append(h('div', { class: 'page full' },
    h('div', { class: 'editor' },
      h('div', { class: 'editor-left' },
        h('div', { class: 'row' }, h('a', { href: '#/teams' }, '← Teams'), h('h1', { style: 'margin:0 0 0 8px' }, isNew ? 'New team' : team.name), dirtyEl,
          h('span', { class: 'spacer' }),
          isNew ? null : h('a', { class: 'btn', href: '#/?team=' + team.id }, 'Start session'),
          h('button', { class: 'primary', onclick: save }, 'Save team')),
        h('div', { class: 'card' },
          h('div', { class: 'team-meta' },
            h('div', {}, h('label', {}, 'Name'), f.name),
            h('div', {}, h('label', {}, 'Id'), f.id),
            h('div', {}, h('label', {}, 'Entry persona (receives the task)'), f.entry),
            h('div', {}, h('label', {}, 'Max turns'), f.max_turns),
            h('div', {}, h('label', {}, 'Max parallel'), f.max_parallel)),
          h('label', {}, 'Description'), f.description),
        h('div', { class: 'toolbar' },
          h('button', { onclick: addPersona }, '+ Persona'), connectBtn,
          h('label', { class: 'inline small' }, bidiBox, 'new channels are bidirectional'),
          h('span', { class: 'spacer' }),
          h('button', { onclick: relayout }, 'Auto layout'), h('button', { onclick: () => graph.fit() }, 'Fit')),
        graphWrap),
      side)));
}

function newPersona(id, name, rts) {
  const rt = rts.find(r => r.name === 'claude') || rts[0];
  return { id, name, role: '', runtime: rt.name, model: rt.models[0] || '', effort: 'medium', prompt: '' };
}

// ---------------------------------------------------------------- settings

const PRESETS = {
  DeepSeek: { api: 'openai', base_url: 'https://api.deepseek.com', model: 'deepseek-chat', api_key_env: 'DEEPSEEK_API_KEY' },
  OpenAI: { api: 'openai', base_url: 'https://api.openai.com/v1', model: 'gpt-5.5', api_key_env: 'OPENAI_API_KEY' },
  Anthropic: { api: 'anthropic', base_url: 'https://api.anthropic.com', model: 'claude-sonnet-5-5', api_key_env: 'ANTHROPIC_API_KEY' },
  OpenRouter: { api: 'openai', base_url: 'https://openrouter.ai/api/v1', model: 'deepseek/deepseek-chat', api_key_env: 'OPENROUTER_API_KEY' },
  'Ollama (local)': { api: 'openai', base_url: 'http://localhost:11434/v1', model: 'llama3.3', api_key_env: 'OLLAMA_API_KEY' },
};

const JEV_PROVIDERS = {
  OpenRouter: { base_url: 'https://openrouter.ai/api/v1', api_key_env: 'JEV_API_KEY' },
  'TypeSafe direct': { base_url: 'https://api.typesafe.ai/v1', api_key_env: 'TYPESAFE_API_KEY' },
};

/** Settings for quick orchestration decisions with Jev. */
function jevCard(settings) {
  const d = settings.decisions;
  const num = (v, step = '0.05') => h('input', { type: 'number', step, min: 0, max: 1, value: v, style: 'width:90px' });
  const f = {
    enabled: h('input', { type: 'checkbox', checked: d.enabled }),
    base_url: h('input', { value: d.base_url }),
    model: h('input', { value: d.model }),
    api_key_env: h('input', { value: d.api_key_env, class: 'mono' }),
    timeout_ms: h('input', { type: 'number', min: 100, step: 100, value: d.timeout_ms, style: 'width:110px' }),
    ack_filter: h('input', { type: 'checkbox', checked: d.ack_filter }),
    ack_min_confidence: num(d.ack_min_confidence),
    resolve_recipients: h('input', { type: 'checkbox', checked: d.resolve_recipients }),
    resolve_min_confidence: num(d.resolve_min_confidence),
    detect_final: h('input', { type: 'checkbox', checked: d.detect_final }),
    final_threshold: num(d.final_threshold),
    route_user_messages: h('input', { type: 'checkbox', checked: d.route_user_messages }),
    route_min_confidence: num(d.route_min_confidence),
  };
  const status = h('div', { class: 'small' });
  const showStatus = s => {
    status.className = 'small ' + (s.decisions_active ? 'muted' : 'avail no');
    status.textContent = s.decisions_active
      ? `✔ active, key read from $${s.decisions_key_env}`
      : !s.decisions.enabled ? '✖ disabled' : `✖ no API key: set $${s.decisions.api_key_env} (or JEV_API_KEY / OPENROUTER_API_KEY / TYPESAFE_API_KEY) for the server`;
  };
  showStatus(settings);
  const testOut = h('span', { class: 'muted small' });
  const values = () => {
    const out = {};
    for (const [k, el] of Object.entries(f)) out[k] = el.type === 'checkbox' ? el.checked : el.type === 'number' ? +el.value : el.value;
    return out;
  };
  async function save() {
    try { showStatus(await api('PUT', '/api/settings', { decisions: values() })); toast('Jev settings saved'); }
    catch (e) { toast(e.message, true); }
  }
  async function test() {
    testOut.replaceChildren(h('span', { class: 'spinner' }));
    try {
      await save();
      const r = await api('POST', '/api/decisions/test');
      testOut.textContent = `✔ ${r.model} answered in ${r.latency_ms}ms` + (r.cost != null ? ` ($${r.cost.toFixed(5)})` : '');
    } catch (e) { testOut.textContent = ''; toast(e.message, true); }
  }
  const toggle = (box, text, thr, thrLabel) => h('div', { class: 'row', style: 'margin:8px 0' },
    h('label', { class: 'inline', style: 'flex:1' }, box, text),
    thr ? h('span', { class: 'muted small' }, thrLabel) : null, thr);
  return h('div', { class: 'card', style: 'grid-column: 1 / -1' },
    h('div', { class: 'row between' }, h('h2', { style: 'margin:0' }, '⚡ Quick decisions (Jev)'), h('label', { class: 'inline' }, f.enabled, 'Enabled')),
    h('p', { class: 'muted small' },
      'Jev (TypeSafe’s decision model) makes cheap ~100ms calls between agent turns. Each one only adjusts routing; if Jev is off or fails, Chimera behaves as without it. Decisions appear in the session timeline. Dry runs never call Jev.'),
    status,
    h('div', { class: 'row', style: 'margin-top:10px' }, h('span', { class: 'muted small' }, 'Provider:'),
      Object.entries(JEV_PROVIDERS).map(([name, p]) => h('button', {
        onclick: () => { f.base_url.value = p.base_url; f.api_key_env.value = p.api_key_env; },
      }, name))),
    h('div', { class: 'grid', style: 'grid-template-columns: 2fr 1fr 1fr 120px; gap: 0 10px' },
      h('div', {}, h('label', {}, 'Base URL'), f.base_url),
      h('div', {}, h('label', {}, 'Model'), f.model),
      h('div', {}, h('label', {}, 'API key environment variable'), f.api_key_env),
      h('div', {}, h('label', {}, 'Timeout (ms)'), f.timeout_ms)),
    h('div', { style: 'margin-top:12px' },
      toggle(f.ack_filter, 'Don’t wake a persona for pure acknowledgements (“thanks”, “sounds good”); they are read on its next turn', f.ack_min_confidence, 'min confidence'),
      toggle(f.resolve_recipients, 'Re-address messages sent to an unknown teammate id (e.g. “backend_engineer” → backend)', f.resolve_min_confidence, 'min confidence'),
      toggle(f.detect_final, 'Complete the session when the entry persona reports the task done but forgets <final>', f.final_threshold, 'min probability'),
      toggle(f.route_user_messages, 'Offer “Auto” as recipient of your follow-up messages', f.route_min_confidence, 'min confidence')),
    h('div', { class: 'row', style: 'margin-top:14px' }, testOut, h('span', { class: 'spacer' }),
      h('button', { onclick: test }, 'Test connection'), h('button', { class: 'primary', onclick: save }, 'Save')));
}

async function settingsPage() {
  const [settings, rts] = await Promise.all([api('GET', '/api/settings'), api('GET', '/api/runtimes')]);
  const cfg = settings.prompt_llm;
  const f = {
    api: h('select', {}, h('option', { value: 'openai' }, 'OpenAI-compatible (chat/completions)'), h('option', { value: 'anthropic' }, 'Anthropic Messages')),
    base_url: h('input', { value: cfg.base_url }),
    model: h('input', { value: cfg.model }),
    api_key_env: h('input', { value: cfg.api_key_env, class: 'mono' }),
    temperature: h('input', { type: 'number', step: '0.1', min: 0, max: 2, value: cfg.temperature }),
  };
  f.api.value = cfg.api;
  const keyStatus = h('span', { class: 'small ' + (settings.api_key_present ? 'muted' : 'avail no') }, settings.api_key_present ? '✔ key found in the server environment' : '✖ variable not set in the server environment');
  async function save() {
    try {
      const res = await api('PUT', '/api/settings', {
        prompt_llm: { api: f.api.value, base_url: f.base_url.value, model: f.model.value, api_key_env: f.api_key_env.value, temperature: +f.temperature.value },
      });
      keyStatus.textContent = res.api_key_present ? '✔ key found in the server environment' : '✖ variable not set in the server environment';
      keyStatus.className = 'small ' + (res.api_key_present ? 'muted' : 'avail no');
      toast('Settings saved');
    } catch (e) { toast(e.message, true); }
  }
  $app.append(h('div', { class: 'page' },
    h('div', { class: 'grid two' },
      h('div', { class: 'card' },
        h('h2', {}, 'Prompt generation LLM'),
        h('p', { class: 'muted small' }, 'Used by “Generate prompt” in the team editor. The API key is read from the named environment variable of the chimera server and never stored.'),
        h('div', { class: 'row' }, Object.keys(PRESETS).map(name => h('button', {
          onclick: () => { const p = PRESETS[name]; for (const k of Object.keys(p)) f[k].value = p[k]; },
        }, name))),
        h('label', {}, 'API'), f.api,
        h('label', {}, 'Base URL'), f.base_url,
        h('label', {}, 'Model'), f.model,
        h('label', {}, 'API key environment variable'), f.api_key_env, keyStatus,
        h('label', {}, 'Temperature'), f.temperature,
        h('div', { class: 'row', style: 'margin-top:14px' }, h('span', { class: 'spacer' }), h('button', { class: 'primary', onclick: save }, 'Save'))),
      jevCard(settings),
      h('div', { class: 'card', style: 'grid-column: 1 / -1' },
        h('h2', {}, 'Agent runtimes'),
        h('p', { class: 'muted small' }, 'Configured in chimera.yaml (command templates, output parsers, model suggestions).'),
        h('table', {},
          h('thead', {}, h('tr', {}, ['Runtime', 'Command', 'Status', 'Model list from'].map(c => h('th', {}, c)))),
          h('tbody', {}, rts.map(r => h('tr', {},
            h('td', {}, h('span', { class: 'dot', style: `display:inline-block;margin-right:6px;background:${runtimeColor(r.name)}` }), h('b', {}, r.name), h('div', { class: 'muted small' }, r.label)),
            h('td', { class: 'mono' }, r.command || '—'),
            h('td', {}, r.available ? h('span', { class: 'badge completed' }, 'available') : h('span', { class: 'badge idle' }, 'not on PATH')),
            h('td', { class: 'small muted mono' }, r.models_via)))))))));
}

route();
