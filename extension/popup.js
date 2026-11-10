import { h, icon } from './ui.js';

const app = document.getElementById('app');
const toastEl = document.getElementById('toast');
const send = message => chrome.runtime.sendMessage({ to: 'popup', ...message });

let entries = [];
let site = null; // { host, origin, ids } for the active tab, null off the web
let query = '';
let toastTimer;

function toast(text) {
  toastEl.textContent = text;
  toastEl.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { toastEl.hidden = true; }, 3200);
}

const show = (...nodes) => app.replaceChildren(...nodes.filter(Boolean));

// Any failed request might mean vaultd auto-locked or went away.
async function failed(r) {
  const state = await send({ op: 'state' });
  if (state.state !== 'unlocked') return start(state);
  toast(r.error || 'That did not work. Try again.');
}

function square(name, label, onclick) {
  return h('button', { className: 'square', type: 'button', title: label, 'aria-label': label, onclick }, icon(name));
}

function flash(button) {
  button.replaceChildren(icon('check'));
  button.classList.add('done');
  setTimeout(() => { button.replaceChildren(icon('copy')); button.classList.remove('done'); }, 1200);
}

async function copySecret(id, button) {
  const r = await send({ op: 'get', id });
  if (!r.ok) return failed(r);
  await navigator.clipboard.writeText(r.data.password);
  await send({ op: 'arm-clear' });
  flash(button);
  toast('Password copied. The clipboard clears in 30 s.');
}

async function fill(id) {
  const r = await send({ op: 'fill', id });
  if (r.ok) window.close();
  else toast(r.error || 'Could not fill this page.');
}

function revealField(input) {
  const toggle = square('eye', 'Show or hide', () => { input.type = input.type === 'password' ? 'text' : 'password'; });
  return h('div', { className: 'reveal-field' }, input, toggle);
}

// Gates: setup, create, unlock

function gate(title, detail, ...body) {
  show(h('section', { className: 'gate' },
    h('div', { className: 'brand' }, icon('mark')),
    h('div', { className: 'stack' }, h('h1', {}, title), detail && h('p', { className: 'muted' }, detail)),
    ...body));
}

function setupScreen(kind) {
  gate(kind === 'host' ? 'Connect Nexus to this browser' : 'Nexus could not start',
    kind === 'host'
      ? 'This browser cannot reach nexus-vault-host. Run install.sh from the Nexus source, then try again.'
      : 'The host is installed but nexus-vaultd did not start. Check that Nexus is installed on this computer.',
    h('button', { className: 'primary', onclick: () => start() }, 'Try again'));
}

function createScreen() {
  const pass = h('input', { type: 'password', placeholder: 'Master password', autocomplete: 'new-password', autofocus: true });
  const again = h('input', { type: 'password', placeholder: 'Repeat it', autocomplete: 'new-password' });
  const err = h('p', { className: 'error', hidden: true });
  const fail = text => { err.textContent = text; err.hidden = false; };
  gate('Create your vault', 'One master password opens everything. Nobody can recover it for you.',
    h('form', {
      onsubmit: async e => {
        e.preventDefault();
        if (pass.value.length < 8) return fail('Use at least 8 characters.');
        if (pass.value !== again.value) return fail('The two passwords differ.');
        const r = await send({ op: 'create', password: pass.value });
        if (r.ok) start(); else fail(r.error || 'The vault could not be created.');
      },
    }, revealField(pass), again, err, h('button', { className: 'primary' }, 'Create vault')));
}

function lockedScreen() {
  const pass = h('input', { type: 'password', placeholder: 'Master password', autocomplete: 'current-password', autofocus: true, 'aria-label': 'Master password' });
  const err = h('p', { className: 'error', hidden: true });
  const submit = h('button', { className: 'primary' }, 'Unlock');
  gate('Nexus is locked', null,
    h('form', {
      onsubmit: async e => {
        e.preventDefault();
        if (!pass.value) return;
        submit.disabled = true;
        submit.textContent = 'Unlocking…';
        const r = await send({ op: 'unlock', password: pass.value });
        submit.disabled = false;
        submit.textContent = 'Unlock';
        if (r.ok) return start();
        err.textContent = r.setup ? 'Nexus is not reachable.' : /wrong/i.test(r.error) ? 'That master password is not correct.' : r.error;
        err.hidden = false;
        pass.select();
      },
    }, revealField(pass), err, submit));
}

// The list

const byTitle = (a, b) => (a.title || '').localeCompare(b.title || '', undefined, { sensitivity: 'base' });

function matchesQuery(entry, q) {
  return [entry.title, entry.username, ...(entry.urls || [])].some(v => (v || '').toLowerCase().includes(q));
}

function row(entry, onSite) {
  const copy = square('copy', 'Copy password', () => copySecret(entry.id, copy));
  return h('div', { className: 'row' },
    h('button', { className: 'row-main', title: entry.title, onclick: () => detailScreen(entry.id) },
      h('span', { className: 'row-title' }, entry.title || 'Untitled'),
      h('span', { className: 'row-sub' }, entry.username || 'No username')),
    onSite && h('button', { className: 'action', onclick: () => fill(entry.id) }, 'Fill'),
    copy);
}

function listBody() {
  const q = query.trim().toLowerCase();
  const ids = new Set(site?.ids);
  if (q) {
    const hits = entries.filter(e => matchesQuery(e, q));
    if (!hits.length) return [h('div', { className: 'inline-empty' }, `Nothing matches “${query.trim()}”.`, h('button', { onclick: () => editScreen({ title: query.trim() }) }, 'Add'))];
    return hits.map(e => row(e, ids.has(e.id)));
  }
  if (!entries.length) {
    return [h('section', { className: 'gate' },
      h('div', { className: 'stack' }, h('h1', {}, 'Your vault is empty'), h('p', { className: 'muted' }, 'Add a login here, or sign in on a site and Nexus will offer to save it.')),
      h('button', { className: 'primary', onclick: () => editScreen() }, 'Add a login'))];
  }
  const local = entries.filter(e => ids.has(e.id));
  const nodes = [];
  if (site) {
    nodes.push(h('div', { className: 'section', title: site.host }, `On ${site.host}`));
    if (local.length) nodes.push(...local.map(e => row(e, true)));
    else nodes.push(h('div', { className: 'inline-empty' }, 'No login saved for this site.', h('button', { onclick: () => editScreen() }, 'Add')));
    if (entries.length > local.length) nodes.push(h('div', { className: 'section' }, 'Other logins'));
  }
  nodes.push(...entries.filter(e => !ids.has(e.id)).map(e => row(e, false)));
  return nodes;
}

async function listScreen() {
  const [list, here] = await Promise.all([send({ op: 'list' }), send({ op: 'site' })]);
  if (!list.ok) return failed(list);
  entries = list.data.sort(byTitle);
  site = here.ok ? here.data : null;
  const scroller = h('div', { className: 'scroller' });
  const fillBody = () => scroller.replaceChildren(...listBody());
  const search = h('input', {
    type: 'search', placeholder: `Search ${entries.length} ${entries.length === 1 ? 'login' : 'logins'}`, value: query, autofocus: true, 'aria-label': 'Search logins',
    oninput: () => { query = search.value; fillBody(); },
  });
  show(h('div', { className: 'bar' },
    h('div', { className: 'search' }, icon('search'), search),
    square('plus', 'Add a login', () => editScreen()),
    square('lock', 'Lock', async () => { await send({ op: 'lock' }); start(); })),
    scroller);
  fillBody();
  search.focus();
}

// Detail

async function detailScreen(id) {
  const r = await send({ op: 'get', id });
  if (!r.ok) return failed(r);
  const entry = r.data;
  const copyUser = square('copy', 'Copy username', async () => {
    await navigator.clipboard.writeText(entry.username);
    flash(copyUser);
  });
  const copyPass = square('copy', 'Copy password', () => copySecret(id, copyPass));
  const secret = h('span', { className: 'value mono' }, '•'.repeat(Math.min(entry.password.length, 24)));
  let revealed = false;
  const reveal = square('eye', 'Show or hide password', () => {
    revealed = !revealed;
    secret.textContent = revealed ? entry.password : '•'.repeat(Math.min(entry.password.length, 24));
    secret.style.whiteSpace = revealed ? 'normal' : 'nowrap';
    secret.style.wordBreak = revealed ? 'break-all' : 'normal';
  });
  show(
    h('div', { className: 'bar' },
      square('back', 'Back', listScreen),
      h('h1', { title: entry.title }, entry.title || 'Untitled'),
      h('button', { onclick: () => editScreen(entry) }, 'Edit')),
    h('div', { className: 'detail' },
      entry.username && field('Username', h('div', { className: 'field-value' }, h('span', { className: 'value' }, entry.username), copyUser)),
      field('Password', h('div', { className: 'field-value' }, secret, reveal, copyPass)),
      entry.urls?.length ? field('Websites', h('div', { className: 'urls' }, entry.urls.map(u => h('span', { title: u, className: 'muted' }, u)))) : null,
      entry.notes && field('Notes', h('p', { className: 'notes muted' }, entry.notes))),
  );
}

function field(label, ...content) {
  return h('div', { className: 'field-block' }, h('span', { className: 'field-label' }, label), ...content);
}

// Add and edit

function editScreen(entry = {}) {
  const editing = !!entry.id;
  const defaults = !editing && site ? { title: entry.title || site.host, urls: [site.origin] } : entry;
  const title = h('input', { value: defaults.title || '', required: true });
  const username = h('input', { value: defaults.username || '', autocomplete: 'off' });
  const password = h('input', { type: 'password', value: defaults.password || '', autocomplete: 'off', className: 'mono', required: true });
  const urls = h('textarea', { value: (defaults.urls || []).join('\n'), placeholder: 'https://example.com' });
  const notes = h('textarea', { value: defaults.notes || '' });
  const panel = h('div', { className: 'generator', hidden: true });

  const generate = async () => {
    const r = await send({ op: 'generate', length: +length.value, symbols: symbols.checked });
    if (!r.ok) return failed(r);
    password.value = r.data.password;
    password.type = 'text';
  };
  const count = h('span', { className: 'count' }, '20');
  const length = h('input', { type: 'range', min: 12, max: 64, value: 20, 'aria-label': 'Length', oninput: () => { count.textContent = length.value; generate(); } });
  const symbols = h('input', { type: 'checkbox', checked: true, onchange: generate });
  panel.append(
    h('div', { className: 'line' }, h('span', {}, 'Length'), length, count),
    h('label', { className: 'line' }, symbols, h('span', {}, 'Include symbols')),
    h('button', { type: 'button', onclick: generate }, 'Generate another'));

  const pair = h('div', { className: 'pair' }, revealField(password),
    square('refresh', 'Generate a password', () => { panel.hidden = !panel.hidden; if (!panel.hidden) generate(); }));

  let confirming = false;
  const remove = editing && h('button', {
    type: 'button', className: 'flat danger',
    onclick: async () => {
      if (!confirming) {
        confirming = true;
        remove.textContent = 'Confirm delete';
        setTimeout(() => { confirming = false; remove.textContent = 'Delete'; }, 3000);
        return;
      }
      const r = await send({ op: 'delete', id: entry.id });
      if (r.ok) listScreen(); else failed(r);
    },
  }, 'Delete');

  show(
    h('div', { className: 'bar' },
      square('back', 'Back', () => editing ? detailScreen(entry.id) : listScreen()),
      h('h1', {}, editing ? 'Edit login' : 'New login')),
    h('form', {
      className: 'editor',
      onsubmit: async e => {
        e.preventDefault();
        const r = await send({
          op: 'save', ...(editing ? { id: entry.id } : {}),
          title: title.value.trim(), username: username.value, password: password.value, notes: notes.value,
          urls: urls.value.split('\n').map(u => u.trim()).filter(Boolean),
        });
        if (r.ok) listScreen(); else failed(r);
      },
    },
    h('div', { className: 'detail' },
      block('Title', title), block('Username', username),
      block('Password', pair), panel,
      block('Websites', urls), block('Notes', notes)),
    h('div', { className: 'footer' },
      remove, h('span', { className: 'spacer' }),
      h('button', { className: 'flat', type: 'button', onclick: () => editing ? detailScreen(entry.id) : listScreen() }, 'Cancel'),
      h('button', { className: 'primary' }, 'Save'))),
  );
  title.focus();
}

function block(label, control) {
  return h('label', { className: 'field-block' }, h('span', { className: 'field-label' }, label), control);
}

async function start(state) {
  const s = state || await send({ op: 'state' });
  if (s.state === 'setup') return setupScreen(s.setup);
  if (s.state === 'none') return createScreen();
  if (s.state === 'locked') return lockedScreen();
  return listScreen();
}

start();
