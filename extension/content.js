// Runs in the isolated world. Holds no credential until the service worker
// delivers one in response to a click inside the extension-origin menu.
(() => {
  const USER_HINT = /user|login|email|e-mail|mail|phone|identifi|account/i;
  const NOT_LOGIN = /otp|one.?time|2fa|totp|verif|captcha|cvv|cvc|card|coupon|promo|search|query/i;
  const NEW_HINT = /new|confirm|repeat|retype|verify|sign.?up|regist|create/i;
  const NEWSLETTER = /newsletter|subscribe/i;
  const SUBMIT_TEXT = /log.?in|sign.?in|sign.?up|submit|continue|next|create|register|connexion|se connecter/i;

  let allowed; // undefined until the worker has judged this frame
  let target = null; // field the UI is attached to
  let host, root, iconEl, frame, hideTimer;

  const attr = (el, name) => (el.getAttribute(name) || '').toLowerCase();
  const hintOf = el => `${el.name} ${el.id} ${attr(el, 'placeholder')} ${attr(el, 'aria-label')}`;

  function visible(el) {
    if (!el.isConnected || !el.checkVisibility({ checkOpacity: true, checkVisibilityCSS: true })) return false;
    const r = el.getBoundingClientRect();
    return r.width >= 20 && r.height >= 8;
  }

  const scopeOf = el => el.form || el.getRootNode();
  const inputsIn = scope => [...scope.querySelectorAll('input')].filter(visible);
  const passwordsIn = scope => inputsIn(scope).filter(i => i.type === 'password');
  const textLike = el => ['text', 'email', 'tel'].includes(el.type);

  function kindOf(el) {
    if (!(el instanceof HTMLInputElement) || el.disabled || el.readOnly || !visible(el)) return null;
    const ac = attr(el, 'autocomplete');
    const hint = hintOf(el);
    if (ac.includes('one-time-code') || NOT_LOGIN.test(hint)) return null;
    if (el.type === 'password') {
      const siblings = passwordsIn(scopeOf(el));
      return ac.includes('new-password') || NEW_HINT.test(hint) || siblings.length > 1 ? 'new' : 'password';
    }
    if (!textLike(el) || attr(el, 'role') === 'searchbox') return null;
    if (ac.includes('username')) return 'username';
    if (!USER_HINT.test(hint) && el.type !== 'email') return null;
    const scope = scopeOf(el);
    if (passwordsIn(scope).length) return 'username';
    const fields = inputsIn(scope).filter(textLike);
    const formText = scope instanceof HTMLFormElement ? `${attr(scope, 'action')} ${scope.id} ${scope.className}` : '';
    return fields.length === 1 && scope instanceof HTMLFormElement && !NEWSLETTER.test(formText) ? 'username' : null;
  }

  // Everything the page can observe here is a host element and its box.
  function ensureHost() {
    if (host?.isConnected) return;
    host = document.createElement('nexus-vault');
    root = host.attachShadow({ mode: 'closed' });
    const sheet = new CSSStyleSheet();
    sheet.replaceSync(`
      :host { all: initial !important; position: fixed !important; inset: 0 auto auto 0 !important;
        width: 0 !important; height: 0 !important; margin: 0 !important; padding: 0 !important; border: 0 !important;
        overflow: visible !important; background: transparent !important; z-index: 2147483647 !important; }
      button { all: initial; position: fixed; width: 22px; height: 22px; display: grid; place-items: center;
        border-radius: 6px; cursor: pointer; opacity: .62; transition: opacity 80ms ease, background-color 80ms ease; }
      button:hover { opacity: 1; background: color-mix(in srgb, currentColor 12%, transparent); }
      svg { width: 16px; height: 16px; display: block; }
      iframe { position: fixed; border: 0; border-radius: 12px; width: 320px; height: 0; color-scheme: light dark;
        box-shadow: 0 14px 32px -12px rgba(0,0,0,.55), 0 0 0 1px rgba(0,0,0,.28); }`);
    root.adoptedStyleSheets = [sheet];
    iconEl = document.createElement('button');
    iconEl.type = 'button';
    iconEl.tabIndex = -1;
    iconEl.ariaLabel = 'Open Nexus Passwords';
    iconEl.style.display = 'none'; // place() shows it once there is a field
    iconEl.append(mark());
    iconEl.addEventListener('mousedown', e => e.preventDefault());
    iconEl.addEventListener('click', e => { if (e.isTrusted) toggleMenu(); });
    root.append(iconEl);
    host.popover = 'manual';
    (target?.closest('dialog') || document.documentElement).append(host);
    host.showPopover();
  }

  function mark() {
    const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    svg.setAttribute('viewBox', '0 0 16 16');
    svg.setAttribute('fill', 'none');
    svg.setAttribute('stroke', 'currentColor');
    svg.setAttribute('stroke-width', '1.5');
    svg.setAttribute('stroke-linecap', 'round');
    svg.setAttribute('stroke-linejoin', 'round');
    const path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
    path.setAttribute('d', 'M3 13V3l10 10V3');
    svg.append(path);
    return svg;
  }

  function place() {
    if (!target || !host?.isConnected) return;
    const r = target.getBoundingClientRect();
    if (!visible(target) || r.bottom < 0 || r.top > innerHeight) { iconEl.style.display = 'none'; return; }
    iconEl.style.display = 'grid';
    iconEl.style.color = getComputedStyle(target).color;
    let right = r.right - 26;
    // Asked of the field's own root: the document would answer with a shadow host.
    const probe = target.getRootNode().elementFromPoint(r.right - 12, r.top + r.height / 2);
    if (probe && probe !== target && probe !== host && !probe.contains(target)) right -= 26;
    iconEl.style.left = `${right}px`;
    iconEl.style.top = `${r.top + (r.height - 22) / 2}px`;
    if (frame && !frame.dataset.offer) placeFrame(r);
  }

  function placeFrame(r) {
    const height = frame.offsetHeight;
    const width = 320;
    frame.style.left = `${Math.max(8, Math.min(r.right - width, innerWidth - width - 8))}px`;
    const below = r.bottom + 6;
    frame.style.top = `${below + height > innerHeight - 8 && r.top - height - 6 > 8 ? r.top - height - 6 : below}px`;
  }

  function attach(el) {
    if (allowed === false) return;
    clearTimeout(hideTimer);
    if (target === el) return;
    if (allowed === undefined) {
      chrome.runtime.sendMessage({ type: 'allowed' }).then(r => {
        allowed = !!r?.ok;
        if (allowed) attach(el);
      });
      allowed = null;
      return;
    }
    if (allowed === null) return;
    closeFrame();
    target = el;
    ensureHost();
    resizes.disconnect();
    resizes.observe(el);
    place();
  }

  const resizes = new ResizeObserver(() => place());
  addEventListener('scroll', place, { capture: true, passive: true });
  addEventListener('resize', place, { passive: true });

  function detach() {
    if (frame) return;
    hideTimer = setTimeout(() => {
      target = null;
      if (iconEl) iconEl.style.display = 'none';
      resizes.disconnect();
    }, 400);
  }

  function closeFrame() {
    frame?.remove();
    frame = null;
  }

  function openFrame(token, offer) {
    ensureHost();
    closeFrame();
    frame = document.createElement('iframe');
    frame.src = `chrome-extension://${chrome.runtime.dynamicId}/menu.html#${token}`;
    if (offer) {
      frame.dataset.offer = '1';
      frame.style.right = '16px';
      frame.style.top = '16px';
      frame.style.left = 'auto';
    } else {
      placeFrame(target.getBoundingClientRect());
    }
    root.append(frame);
  }

  async function toggleMenu() {
    if (frame && !frame.dataset.offer) return closeFrame();
    const field = kindOf(target) || 'password';
    const r = await chrome.runtime.sendMessage({ type: 'open-menu', field });
    if (r?.ok && target) openFrame(r.token, false);
  }

  function deepActive() {
    let el = document.activeElement;
    while (el?.shadowRoot?.activeElement) el = el.shadowRoot.activeElement;
    return el;
  }

  const eventInput = e => e.composedPath()[0];

  addEventListener('focusin', e => {
    if (kindOf(eventInput(e))) attach(eventInput(e));
  }, true);
  addEventListener('pointerover', e => {
    const el = eventInput(e);
    if (el instanceof HTMLInputElement && kindOf(el)) attach(el);
  }, { capture: true, passive: true });
  addEventListener('focusout', e => { if (eventInput(e) === target) detach(); }, true);
  addEventListener('pointerout', e => { if (eventInput(e) === target && deepActive() !== target) detach(); }, true);
  addEventListener('pointerdown', e => {
    if (frame && e.isTrusted && !e.composedPath().includes(host)) closeFrame();
  }, true);

  function usernameFor(pw, scope) {
    const fields = inputsIn(scope);
    return fields.slice(0, fields.indexOf(pw)).reverse().find(textLike);
  }

  function setValue(el, value) {
    el.focus();
    // In the isolated world this setter is the browser's own, not the page's
    // instance override, so React's value tracker sees a genuine change.
    el.value = value;
    el.dispatchEvent(new InputEvent('input', { bubbles: true, composed: true, inputType: 'insertReplacementText' }));
    el.dispatchEvent(new Event('change', { bubbles: true, composed: true }));
  }

  function loginField() {
    const active = deepActive();
    if (active instanceof HTMLInputElement && kindOf(active)) return active;
    return passwordsIn(document).find(i => kindOf(i) === 'password') || passwordsIn(document)[0]
      || inputsIn(document).find(i => kindOf(i) === 'username');
  }

  function fill({ username, password, generated }) {
    const field = target?.isConnected && visible(target) ? target : loginField();
    if (!field) return false;
    const scope = scopeOf(field);
    const passwords = passwordsIn(scope);
    if (generated) {
      const fresh = passwords.filter(p => kindOf(p) === 'new');
      for (const p of fresh.length ? fresh : passwords) setValue(p, password);
      return passwords.length > 0;
    }
    const pw = field.type === 'password' ? field : passwords.find(p => kindOf(p) !== 'new') || passwords[0];
    const user = field.type !== 'password' ? field : pw && usernameFor(pw, scope);
    // A password is only ever written to a field that is a password field now.
    if (user && username && textLike(user)) setValue(user, username);
    if (pw && pw.type === 'password') setValue(pw, password);
    (pw || user)?.focus();
    return !!(pw || user);
  }

  function snapshot(scope) {
    const pw = passwordsIn(scope).find(p => p.value);
    if (!pw) return;
    const confirm = passwordsIn(scope).filter(p => p.value);
    const chosen = confirm.length >= 2 ? confirm[confirm.length - 1] : pw;
    const user = usernameFor(pw, scope);
    chrome.runtime.sendMessage({ type: 'submitted', username: user?.value || '', password: chosen.value });
    // A page that stays put offers once the form has gone; one that navigates
    // is handled by the worker when the next document completes.
    for (const ms of [1500, 4000]) {
      setTimeout(() => { if (!visible(pw)) chrome.runtime.sendMessage({ type: 'offer-now' }); }, ms);
    }
  }

  addEventListener('submit', e => { if (e.target instanceof HTMLFormElement) snapshot(e.target); }, true);
  addEventListener('click', e => {
    if (!e.isTrusted) return;
    const el = e.composedPath()[0];
    const button = el.closest?.('button, input[type=submit], [role=button]');
    if (!button || e.composedPath().includes(host)) return;
    const form = button.form || button.closest('form');
    if (form) { if (button.type === 'submit' && !SUBMIT_TEXT.test(button.textContent + button.value)) return; }
    else if (!SUBMIT_TEXT.test(button.textContent + (button.value || ''))) return;
    snapshot(form || button.getRootNode());
  }, true);
  addEventListener('keydown', e => {
    const el = eventInput(e);
    if (e.isTrusted && e.key === 'Enter' && el instanceof HTMLInputElement && el.type === 'password' && !el.form) snapshot(el.getRootNode());
  }, true);

  const active = deepActive();
  if (active instanceof HTMLInputElement && kindOf(active)) attach(active);

  chrome.runtime.onMessage.addListener((m, sender, reply) => {
    if (sender.id !== chrome.runtime.id || sender.tab) return;
    if (m.type === 'fill') {
      const ok = fill(m);
      closeFrame();
      reply({ ok });
    } else if (m.type === 'resize' && frame) {
      frame.style.height = `${Math.min(m.height, 360)}px`;
      if (!frame.dataset.offer && target) placeFrame(target.getBoundingClientRect());
    } else if (m.type === 'close') {
      closeFrame();
      target?.focus();
    } else if (m.type === 'show-offer') {
      openFrame(m.token, true);
    } else if (m.type === 'open-from-command') {
      const el = deepActive();
      if (el instanceof HTMLInputElement && kindOf(el)) {
        attach(el);
        toggleMenu();
      }
    }
  });
})();
