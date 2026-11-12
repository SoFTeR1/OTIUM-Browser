// Injected into every page loaded in an OTIUM tab (top frame only).
(() => {
  if (window.top !== window || window.__otium) return;
  window.__otium = true;
  const send = (o) => { try { window.ipc.postMessage(JSON.stringify(o)); } catch (_) {} };

  // Browser-level shortcuts must work even when the page has focus.
  window.addEventListener('keydown', (e) => {
    const c = e.ctrlKey || e.metaKey;
    const key = e.key.toLowerCase();
    let k = null;
    if (c && !e.shiftKey && !e.altKey) {
      if (key === 't') k = 'new_tab';
      else if (key === 'w') k = 'close_tab';
      else if (key === 'l') k = 'focus_address';
      else if (key === 'h') k = 'history';
      else if (key === 'j') k = 'downloads';
      else if (key === 'd') k = 'bookmark';
      else if (e.key === ',') k = 'settings';
      else if (e.key === '=' || e.key === '+') k = 'zoom_in';
      else if (e.key === '-') k = 'zoom_out';
      else if (e.key === '0') k = 'zoom_reset';
      else if (e.key >= '1' && e.key <= '9') k = 'tab_' + e.key;
    }
    if (c && e.shiftKey && !e.altKey) {
      if (key === 't') k = 'reopen_tab';
      else if (key === 'n') k = 'new_private';
      else if (key === 'b') k = 'toggle_sidebar';
      else if (key === 'o') k = 'bookmarks';
    }
    if (c && e.key === 'Tab') k = e.shiftKey ? 'prev_tab' : 'next_tab';
    if (e.key === 'F11') k = 'fullscreen';
    if (e.key === 'F12') k = 'devtools';
    if (e.key === 'Escape' && !e.defaultPrevented) send({ cmd: 'key', k: 'exit_fullscreen' });
    if (k) {
      e.preventDefault();
      e.stopImmediatePropagation();
      send({ cmd: 'key', k });
    }
  }, true);

  // Clicking the page closes any open browser popover (menus, extension popups).
  window.addEventListener('mousedown', () => send({ cmd: 'dismiss' }), true);

  // Report the page favicon so the tab strip can show it.
  const reportIcon = () => {
    const links = [...document.querySelectorAll('link[rel~="icon"], link[rel="shortcut icon"], link[rel="apple-touch-icon"]')];
    const best = links.find((l) => /svg|32|any/.test((l.sizes && l.sizes.value) + l.type + l.href)) || links[0];
    let href = best ? best.href : '';
    if (!href && /^https?:$/.test(location.protocol) && location.hostname !== 'otium.localhost') {
      href = location.origin + '/favicon.ico';
    }
    if (href) send({ cmd: 'favicon', href });
  };

  // OTIUM Shield cosmetic filtering: report class/id names, receive CSS that hides ad slots.
  window.__otiumCss = (css) => {
    let s = document.getElementById('__otium_shield');
    if (!s) {
      s = document.createElement('style');
      s.id = '__otium_shield';
      (document.head || document.documentElement).appendChild(s);
    }
    s.textContent = css;
  };
  const reportCosmetic = () => {
    if (!/^https?:$/.test(location.protocol) || location.hostname === 'otium.localhost') return;
    const classes = new Set(), ids = new Set();
    for (const el of document.querySelectorAll('[class],[id]')) {
      if (el.id) ids.add(el.id);
      for (const c of el.classList) classes.add(c);
      if (classes.size > 3000) break;
    }
    send({ cmd: 'cosmetic', classes: [...classes], ids: [...ids] });
  };

  const onReady = () => { reportIcon(); reportCosmetic(); };
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', onReady, { once: true });
  else onReady();

  window.otiumNavigate = (text) => send({ cmd: 'navigate', text });
  // Privileged commands are only honoured for internal otium:// pages (checked in Rust).
  window.otiumSend = send;
})();
