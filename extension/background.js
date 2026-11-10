// The only context that talks to the native host. Every decision about which
// site may receive what is made here, from browser-supplied sender data.
// Nothing here is ever written to chrome.storage: sessions and pending logins
// live in memory and die with the worker.
const HOST = 'io.github.vitrixxl.nexus';
const sessions = new Map(); // token -> frame an in-page menu belongs to
const pending = new Map(); // tabId -> login waiting for a save decision
const PENDING_MS = 120000;

// Same list as SUFFIXES in src/vault.rs, so a frame is judged as vaultd judges a match.
const MULTIPART = new Set([
  'ac.uk', 'co.uk', 'gov.uk', 'ltd.uk', 'me.uk', 'net.uk', 'org.uk', 'plc.uk', 'sch.uk', 'ac.jp', 'co.jp',
  'go.jp', 'ne.jp', 'or.jp', 'co.kr', 'or.kr', 'com.au', 'edu.au', 'gov.au', 'net.au', 'org.au', 'co.nz',
  'net.nz', 'org.nz', 'ac.nz', 'co.za', 'org.za', 'co.in', 'net.in', 'org.in', 'gov.in', 'ac.in', 'com.br',
  'net.br', 'org.br', 'gov.br', 'com.cn', 'net.cn', 'org.cn', 'edu.cn', 'gov.cn', 'com.hk', 'com.mx',
  'com.tw', 'com.sg', 'com.tr', 'com.ar', 'com.pl', 'com.ru', 'com.ua', 'co.il', 'co.id', 'co.th',
  'github.io', 'gitlab.io', 'vercel.app', 'netlify.app', 'pages.dev', 'web.app', 'workers.dev',
  'firebaseapp.com', 'herokuapp.com', 'blogspot.com', 'azurewebsites.net', 'cloudfront.net'
]);

function site(url) {
  const { protocol, hostname } = new URL(url);
  const labels = hostname.split('.');
  if (hostname === 'localhost' || /^[\d.]+$/.test(hostname) || hostname.includes(':') || labels.length < 3) return protocol + hostname;
  return protocol + labels.slice(MULTIPART.has(labels.slice(-2).join('.')) ? -3 : -2).join('.');
}

const web = url => /^https?:/.test(url);

async function native(message) {
  try {
    const reply = await chrome.runtime.sendNativeMessage(HOST, message);
    if (!reply) return { ok: false, setup: 'host' };
    if (reply.ok === false && /vaultd unavailable/.test(reply.error)) return { ok: false, setup: 'daemon' };
    return reply;
  } catch (e) {
    return { ok: false, setup: 'host', error: e.message };
  }
}

// A frame may be offered credentials only if it and every ancestor are the
// same site as itself, and it is not a sandboxed or opaque document.
async function trustFrame(sender) {
  const { tab, frameId, documentId, url, origin } = sender;
  if (!tab || !url || !web(url) || new URL(url).origin !== origin) return null;
  const frames = await chrome.webNavigation.getAllFrames({ tabId: tab.id });
  const byId = new Map(frames.map(f => [f.frameId, f]));
  const mine = site(url);
  for (let f = byId.get(frameId); f; f = byId.get(f.parentFrameId)) {
    if (!web(f.url) || site(f.url) !== mine) return null;
  }
  return byId.has(frameId) ? { tabId: tab.id, frameId, documentId, url } : null;
}

function openSession(frame, extra) {
  for (const [token, s] of sessions) if (s.tabId === frame.tabId) sessions.delete(token);
  const token = crypto.randomUUID();
  sessions.set(token, { ...frame, ...extra });
  return token;
}

const isPopup = s => s.url === chrome.runtime.getURL('popup.html');

// The token is unguessable and reaches only the closed shadow root the menu
// frame lives in. webNavigation does not report extension frames, so the frame
// itself cannot be checked against its parent beyond its tab.
function menuSession(sender, token) {
  const s = sessions.get(token);
  if (!s || sender.tab?.id !== s.tabId || !sender.url?.startsWith('chrome-extension://')
    || new URL(sender.url).pathname !== '/menu.html') return null;
  return s;
}

const toFrame = (s, message) => chrome.tabs.sendMessage(s.tabId, message, { documentId: s.documentId });

async function vaultState() {
  const r = await native({ op: 'status' });
  if (r.setup) return { state: 'setup', setup: r.setup };
  if (!r.ok) return { state: 'setup', setup: 'daemon' };
  return { state: !r.data.exists ? 'none' : r.data.unlocked ? 'unlocked' : 'locked', count: r.data.count };
}

const label = url => new URL(url).hostname.replace(/^www\./, '');

async function menuState(s) {
  const v = await vaultState();
  if (s.mode === 'offer') {
    const p = pending.get(s.tabId);
    if (!p) return { state: 'gone' };
    if (v.state !== 'unlocked') return { state: 'offer-locked', site: label(p.url) };
    return { state: 'offer', kind: p.updateId ? 'update' : 'save', site: label(p.url), username: p.username };
  }
  if (v.state !== 'unlocked') return { ...v, site: label(s.url) };
  const r = await native({ op: 'match', url: s.url });
  if (!r.ok) return { state: 'error' };
  return {
    state: 'ready', site: label(s.url), newPassword: s.field === 'new',
    entries: r.data.map(({ id, title, username }) => ({ id, title, username })),
  };
}

// Frame still holds the document the user opened the menu on, or nothing fills.
async function sameDocument(s) {
  const f = await chrome.webNavigation.getFrame({ tabId: s.tabId, frameId: s.frameId });
  return f?.documentId === s.documentId;
}

async function menuAction(s, m) {
  switch (m.action) {
    case 'state': return menuState(s);
    case 'fill': {
      if (!await sameDocument(s)) return { ok: false };
      const r = await native({ op: 'fill', id: m.id, url: s.url });
      if (!r.ok) return { ok: false };
      await toFrame(s, { type: 'fill', username: r.data.username, password: r.data.password });
      sessions.delete(m.token);
      return { ok: true };
    }
    case 'generate': {
      const r = await native({ op: 'generate', length: m.length, symbols: m.symbols });
      if (!r.ok) return { ok: false };
      s.generated = r.data.password;
      return { ok: true, password: r.data.password };
    }
    case 'use-generated': {
      if (!s.generated || !await sameDocument(s)) return { ok: false };
      await toFrame(s, { type: 'fill', password: s.generated, generated: true });
      sessions.delete(m.token);
      return { ok: true };
    }
    case 'resize': await toFrame(s, { type: 'resize', height: m.height }); return { ok: true };
    case 'close': await toFrame(s, { type: 'close' }); return { ok: true };
    case 'unlock':
      return chrome.action.openPopup().then(() => ({ ok: true }), () => ({ ok: false }));
    case 'offer-save': {
      const p = pending.get(s.tabId);
      if (!p || s.mode !== 'offer') return { ok: false };
      const r = await savePending(p);
      if (r.ok) dropPending(s.tabId);
      return { ok: !!r.ok };
    }
    case 'offer-dismiss': dropPending(s.tabId); return { ok: true };
  }
  return { ok: false };
}

async function savePending(p) {
  if (!p.updateId) {
    const host = label(p.url);
    return native({ op: 'save', title: host, urls: [new URL(p.url).origin], username: p.username, password: p.password, notes: '' });
  }
  const old = await native({ op: 'get', id: p.updateId });
  if (!old.ok) return old;
  const { title, urls, notes, username } = old.data;
  return native({ op: 'save', id: p.updateId, title, urls, notes, username, password: p.password });
}

// Offer only what would change the vault: compared here, never shown to anyone.
async function classify(p) {
  const r = await native({ op: 'match', url: p.url });
  if (!r.ok) return false;
  const same = r.data.filter(e => e.username === p.username);
  for (const e of same) {
    const known = await native({ op: 'fill', id: e.id, url: p.url });
    if (known.ok && known.data.password === p.password) return false;
  }
  p.updateId = same[0]?.id;
  return true;
}

// Only ever registered while something is pending, so ordinary browsing does
// not wake the worker on every navigation.
function onNavigated({ tabId, frameId }) {
  if (frameId === 0 && pending.has(tabId)) offer(tabId);
}

function dropPending(tabId) {
  pending.delete(tabId);
  if (!pending.size) chrome.webNavigation.onCompleted.removeListener(onNavigated);
}

function remember(frame, username, password) {
  pending.set(frame.tabId, { url: frame.url, username, password, at: Date.now() });
  chrome.webNavigation.onCompleted.addListener(onNavigated);
  setTimeout(() => {
    if (Date.now() - (pending.get(frame.tabId)?.at ?? Date.now()) >= PENDING_MS) dropPending(frame.tabId);
  }, PENDING_MS);
}

async function offer(tabId) {
  const p = pending.get(tabId);
  if (!p) return;
  const top = await chrome.webNavigation.getFrame({ tabId, frameId: 0 }).catch(() => null);
  if (!top || !web(top.url) || site(top.url) !== site(p.url)) return;
  const v = await vaultState();
  if (v.state === 'unlocked' && !await classify(p)) return dropPending(tabId);
  if (v.state !== 'unlocked' && v.state !== 'locked') return dropPending(tabId);
  const token = openSession({ tabId, frameId: 0, documentId: top.documentId, url: top.url }, { mode: 'offer' });
  chrome.tabs.sendMessage(tabId, { type: 'show-offer', token }, { documentId: top.documentId }).catch(() => {});
}

function notifyVaultChanged() {
  chrome.runtime.sendMessage({ type: 'vault-changed' }).catch(() => {});
  for (const tabId of pending.keys()) offer(tabId);
}

async function activeFrame() {
  const [tab] = await chrome.tabs.query({ active: true, lastFocusedWindow: true });
  if (!tab) return null;
  const f = await chrome.webNavigation.getFrame({ tabId: tab.id, frameId: 0 }).catch(() => null);
  return f && web(f.url) ? { tabId: tab.id, documentId: f.documentId, url: f.url } : null;
}

const POPUP_OPS = new Set(['status', 'create', 'unlock', 'lock', 'change_password', 'list', 'get', 'save', 'delete', 'generate']);

async function popupRequest(m) {
  if (m.op === 'state') return vaultState();
  if (m.op === 'site') {
    const f = await activeFrame();
    if (!f) return { ok: true, data: null };
    const r = await native({ op: 'match', url: f.url });
    return r.ok ? { ok: true, data: { host: label(f.url), origin: new URL(f.url).origin, ids: r.data.map(e => e.id) } } : r;
  }
  if (m.op === 'fill') {
    const f = await activeFrame();
    if (!f) return { ok: false, error: 'This tab has no page to fill.' };
    const r = await native({ op: 'fill', id: m.id, url: f.url });
    if (!r.ok) return { ok: false, error: r.setup ? 'Nexus is not reachable.' : 'This login does not belong to this site.' };
    return chrome.tabs.sendMessage(f.tabId, { type: 'fill', username: r.data.username, password: r.data.password }, { documentId: f.documentId })
      .then(done => done?.ok ? { ok: true } : { ok: false, error: 'No login form found on this page.' },
        () => ({ ok: false, error: 'Reload the page so Nexus can reach it.' }));
  }
  if (m.op === 'arm-clear') {
    chrome.alarms.create('clear-clipboard', { delayInMinutes: 0.5 });
    return { ok: true };
  }
  if (!POPUP_OPS.has(m.op)) return { ok: false, error: 'Unknown request.' };
  const { op, ...rest } = m;
  const r = await native({ op, ...rest });
  if (r.ok && (op === 'unlock' || op === 'create' || op === 'lock')) notifyVaultChanged();
  return r;
}

async function contentRequest(sender, m) {
  if (m.type === 'allowed') return { ok: !!await trustFrame(sender) };
  const frame = await trustFrame(sender);
  if (!frame) return { ok: false };
  if (m.type === 'open-menu') return { ok: true, token: openSession(frame, { field: m.field }) };
  if (m.type === 'submitted' && m.password) {
    remember(frame, m.username ?? '', m.password);
    return { ok: true };
  }
  if (m.type === 'offer-now') { offer(frame.tabId); return { ok: true }; }
  return { ok: false };
}

chrome.runtime.onMessage.addListener((m, sender, reply) => {
  if (sender.id !== chrome.runtime.id) return false;
  const run = async () => {
    if (m.to === 'popup') return isPopup(sender) ? popupRequest(m) : { ok: false };
    if (m.to === 'menu') {
      const s = menuSession(sender, m.token);
      return s ? menuAction(s, m) : { ok: false, state: 'gone' };
    }
    if (m.type === 'clipboard-cleared') return chrome.offscreen.closeDocument().then(() => ({ ok: true }));
    return sender.tab && !sender.url?.startsWith('chrome-extension://') ? contentRequest(sender, m) : { ok: false };
  };
  run().then(reply, () => reply({ ok: false }));
  return true;
});

// An alarm survives the worker going to sleep; a timer would not. 30 s is also
// the shortest alarm Chromium allows.
chrome.alarms.onAlarm.addListener(({ name }) => {
  if (name !== 'clear-clipboard') return;
  chrome.offscreen.createDocument({
    url: 'offscreen.html', reasons: ['CLIPBOARD'], justification: 'Clear a copied password after 30 seconds.',
  }).catch(() => {});
});

chrome.commands.onCommand.addListener(async command => {
  if (command !== 'open-menu') return;
  const [tab] = await chrome.tabs.query({ active: true, lastFocusedWindow: true });
  if (tab) chrome.tabs.sendMessage(tab.id, { type: 'open-from-command' }).catch(() => {});
});
