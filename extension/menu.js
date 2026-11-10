import { h, icon } from './ui.js';

const token = location.hash.slice(1);
const app = document.getElementById('app');
let armed = false;
let view = {};

const send = message => chrome.runtime.sendMessage({ to: 'menu', token, ...message });

// IntersectionObserver v2 reports false if anything covers this frame or fades
// it: the check that makes a clickjacking overlay fail closed.
new IntersectionObserver(([e]) => {
  if (armed === (e.isVisible === true)) return;
  armed = !armed;
  render();
}, { threshold: [0], delay: 100, trackVisibility: true }).observe(document.documentElement);

const guard = fn => e => { if (armed && e.isTrusted) fn(e); };

function resize() {
  send({ action: 'resize', height: app.offsetHeight });
}

function paused() {
  return armed ? null : h('p', { className: 'paused' }, 'Something on this page is covering Nexus, so filling is paused.');
}

function note(title, detail, action) {
  return h('div', { className: 'note' }, h('p', {}, title), detail && h('p', { className: 'muted' }, detail), action || null);
}

function entryRow(entry) {
  return h('div', { className: 'row' },
    h('button', { className: 'row-main', title: entry.title, onclick: guard(() => fill(entry.id)) },
      h('span', { className: 'row-title' }, entry.title || view.site),
      h('span', { className: 'row-sub' }, entry.username || 'No username')));
}

async function fill(id) {
  const r = await send({ action: 'fill', id });
  if (!r?.ok) { view = { state: 'error' }; render(); }
}

async function generate(length = view.generator?.length ?? 20, symbols = view.generator?.symbols ?? true) {
  const r = await send({ action: 'generate', length, symbols });
  if (!r?.ok) return;
  const value = app.querySelector('.gen-value');
  view.generator = { password: r.password, length, symbols };
  // Update in place: re-rendering would drop the slider mid-drag.
  if (value) value.textContent = r.password;
  else render();
}

function generator() {
  const g = view.generator;
  const count = h('span', {}, `${g.length}`);
  const length = h('input', { type: 'range', min: 12, max: 64, value: g.length, 'aria-label': 'Length',
    oninput: () => { count.textContent = length.value; generate(+length.value, symbols.checked); } });
  const symbols = h('input', { type: 'checkbox', checked: g.symbols, onchange: () => generate(+length.value, symbols.checked) });
  return h('div', { className: 'gen' },
    h('div', { className: 'gen-value' }, g.password),
    h('div', { className: 'gen-row' }, h('label', {}, count, length), h('label', { style: 'flex:none' }, symbols, 'Symbols')),
    h('div', { className: 'gen-row' },
      h('button', { className: 'primary', disabled: !armed, onclick: guard(() => send({ action: 'use-generated' })) }, 'Use this password'),
      h('button', { className: 'square', title: 'Generate another', 'aria-label': 'Generate another', onclick: () => generate() }, icon('refresh'))));
}

function header(text) {
  return h('div', { className: 'head' }, h('span', {}, text));
}

function offerView() {
  if (view.state === 'offer-locked') {
    return h('div', { className: 'offer' },
      h('div', { className: 'offer-title' }, `Save the login for ${view.site}?`),
      h('p', { className: 'muted caption' }, 'Unlock Nexus first, then this offer returns.'),
      h('div', { className: 'offer-actions' },
        h('button', { className: 'flat', onclick: guard(() => send({ action: 'offer-dismiss' }).then(close)) }, 'Not now'),
        h('button', { className: 'primary', disabled: !armed, onclick: guard(openUnlock) }, 'Unlock')));
  }
  const updating = view.kind === 'update';
  return h('div', { className: 'offer' },
    h('div', { className: 'offer-title' }, updating ? `Update the password for ${view.username || view.site}?` : `Save this login for ${view.site}?`),
    !updating && view.username ? h('p', { className: 'muted caption' }, view.username) : null,
    h('div', { className: 'offer-actions' },
      h('button', { className: 'flat', onclick: guard(() => send({ action: 'offer-dismiss' }).then(close)) }, 'Not now'),
      h('button', { className: 'primary', disabled: !armed, onclick: guard(() => send({ action: 'offer-save' }).then(close)) }, updating ? 'Update' : 'Save')));
}

const close = () => send({ action: 'close' });

async function openUnlock() {
  const r = await send({ action: 'unlock' });
  if (!r?.ok) { view = { ...view, hint: 'Click the Nexus icon in the toolbar to unlock.' }; render(); }
}

function body() {
  switch (view.state) {
    case undefined: return [];
    case 'offer': case 'offer-locked': return [offerView(), paused()];
    case 'setup': return [note(
      view.setup === 'host' ? 'Nexus is not connected to this browser.' : 'Nexus could not start its password service.',
      view.setup === 'host' ? 'Run install.sh from the Nexus source, then reopen this menu.' : 'Check that Nexus is installed on this computer.')];
    case 'none': return [note('No vault yet.', 'Open the Nexus icon in the toolbar to create one.')];
    case 'locked': return [note('Nexus is locked.', view.hint,
      h('button', { className: 'primary', disabled: !armed, onclick: guard(openUnlock) }, 'Unlock in Nexus'))];
    case 'error': return [note('Nexus could not fill this login.', 'Reload the page and try again.')];
    case 'gone': return [note('This menu has expired.', 'Click the Nexus icon in the field again.')];
    case 'ready': {
      if (view.generator) return [generator(), paused()];
      const rows = view.entries.map(entryRow);
      return [
        header(rows.length ? `Logins for ${view.site}` : view.site),
        view.newPassword
          ? h('div', { className: 'row' }, h('button', { className: 'row-main', onclick: guard(() => generate()) },
            h('span', { className: 'row-title' }, 'Generate a strong password'),
            h('span', { className: 'row-sub' }, '20 characters, random')))
          : null,
        rows.length ? h('div', { className: 'list' }, rows)
          : !view.newPassword && note('No saved login for this site.', 'Sign in as usual and Nexus will offer to save it.'),
        paused(),
      ];
    }
  }
  return [];
}

function render() {
  app.replaceChildren(...body().filter(Boolean));
  requestAnimationFrame(resize);
}

async function load() {
  view = await send({ action: 'state' });
  if (view?.state === 'gone') return close();
  render();
}

addEventListener('keydown', e => {
  if (e.key === 'Escape') return close();
  if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
  const rows = [...app.querySelectorAll('.row-main')];
  if (!rows.length) return;
  const at = rows.indexOf(document.activeElement);
  const next = e.key === 'ArrowDown' ? Math.min(at + 1, rows.length - 1) : Math.max(at - 1, 0);
  rows[at < 0 ? 0 : next].focus();
  e.preventDefault();
});

chrome.runtime.onMessage.addListener(m => { if (m.type === 'vault-changed') load(); });
load();
