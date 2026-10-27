// The tray icon's panel (see the tray menu section of background.rs, and
// trayMenuStyle() / trayInfo() in app.js for where its look and its contents
// come from).
//
// The backend asks for the panel with the launcher's current state; this page
// builds it, measures it, and asks the backend to place the window by the
// cursor. A choice goes back to the backend, which carries it out. While the
// panel is up the launcher keeps reporting (a download's progress, a friend
// coming online, a network picked here), and the panel is patched in place.

(() => {
  const { invoke } = window.__TAURI__.core;
  const { listen } = window.__TAURI__.event;
  const menu = document.getElementById('menu');

  // Only these may be restyled from the launcher's snapshot.
  const STYLE_VARS = ['bg', 'bg-image', 'border', 'radius', 'shadow', 'pad', 'fg', 'font', 'size', 'weight',
                      'item-pad', 'item-radius', 'hover-bg', 'hover-fg', 'sel-weight', 'sel-bg', 'sel-fg',
                      'sep-top', 'sep-bottom', 'sep-shadow', 'sep-bg', 'sep-height', 'sep-margin',
                      'play-font', 'play-style', 'play-weight', 'play-ls'];

  /// The system icon sets in assets/icons, as the launcher names them.
  const ICON_SETS = ['xp', 'win9x', 'vista', 'win7', 'mac9', 'aqua'];

  // The skin's design tokens for buttons, switches and the progress bar
  // (TRAY_TOKENS in app.js). Liquid Glass sends none, and traymenu.css draws
  // glass itself, so one that isn't sent is cleared rather than kept from the
  // last skin.
  const TOKENS = [
    'accent', 'on-accent', 'muted', 'line', 'ok', 'err', 'dur', 'ease',
    'btn-bg', 'btn-fg', 'btn-border', 'btn-radius', 'btn-shadow', 'btn-tshadow', 'btn-font', 'btn-weight',
    'btn-hover-bg', 'btn-hover-fg', 'btn-hover-border', 'btn-hover-shadow',
    'btn-active-bg', 'btn-active-fg', 'btn-active-border', 'btn-active-shadow', 'btn-disabled-fg',
    'pri-bg', 'pri-fg', 'pri-border', 'pri-radius', 'pri-shadow', 'pri-tshadow', 'pri-font', 'pri-weight',
    'pri-hover-bg', 'pri-hover-fg', 'pri-hover-border', 'pri-hover-shadow',
    'pri-active-bg', 'pri-active-border', 'pri-active-shadow',
    'run-bg', 'run-fg', 'run-border', 'run-shadow', 'run-hover-bg',
    'seg-bg', 'seg-border', 'seg-radius', 'seg-shadow', 'seg-pad', 'seg-fg', 'seg-divider', 'seg-btn-radius',
    'seg-active-bg', 'seg-active-fg', 'seg-active-shadow',
    'tgl-w', 'tgl-h', 'tgl-bw', 'tgl-radius', 'tgl-bg', 'tgl-border', 'tgl-shadow', 'tgl-on-bg', 'tgl-on-border',
    'knob', 'knob-inset', 'knob-radius', 'knob-bg', 'knob-border', 'knob-shadow', 'knob-on-bg',
    'bar-h', 'bar-bg', 'bar-border', 'bar-radius', 'bar-shadow', 'bar-pad',
    'bar-fill', 'bar-fill-size', 'bar-fill-radius', 'bar-fill-shadow',
  ];

  const root = document.documentElement.style;
  const safe = (value) => typeof value === 'string' && value.length < 2000;

  function applyStyle(style) {
    if (!style || typeof style !== 'object') return;
    for (const key of STYLE_VARS) {
      if (safe(style[key])) root.setProperty(`--tm-${key}`, style[key]);
    }
    const tokens = style.tokens && typeof style.tokens === 'object' ? style.tokens : {};
    for (const key of TOKENS) {
      if (safe(tokens[key])) root.setProperty(`--sk-${key}`, tokens[key]);
      else root.removeProperty(`--sk-${key}`);
    }
    // The picture in the panel's head sits inside the panel's padding, so its
    // corner is the panel's less that padding, to run parallel to it.
    const radius = parseFloat(style.radius) || 0;
    const pad = parseFloat(String(style.pad || '').split(' ')[0]) || 0;
    root.setProperty('--tm-inner-radius', `${Math.max(0, radius - pad)}px`);
    // The Windows, Steam and Mac skins draw that system's own icons in the
    // launcher; the panel's pages and rows use the same set (traymenu.css).
    if (ICON_SETS.includes(style.icons)) document.body.dataset.icons = style.icons;
    else delete document.body.dataset.icons;
    document.body.classList.toggle('motion', style.motion !== false);
    document.body.classList.toggle('fade', style.fade === true);
    document.body.classList.toggle('glass', style.glass === true);
    // A bar drawn in blocks fills by whole blocks, as the launcher's does
    // (applyBarFill in app.js).
    document.body.classList.toggle('blocky-bar', /repeating-linear-gradient/.test(tokens['bar-fill'] || ''));
  }

  // ── Building blocks ────────────────────────────────────────────────────

  /// An element, its attributes (skipped when null or false) and children
  /// (strings become text, so nothing from the launcher is ever parsed).
  function h(tag, attrs = {}, ...children) {
    const el = document.createElement(tag);
    for (const [name, value] of Object.entries(attrs)) {
      if (value == null || value === false) continue;
      el.setAttribute(name, value === true ? '' : String(value));
    }
    for (const child of children.flat()) {
      if (child == null || child === false) continue;
      el.append(typeof child === 'string' ? document.createTextNode(child) : child);
    }
    return el;
  }

  const PATHS = {
    play: 'M8 5.14v13.72a1 1 0 0 0 1.52.85l10.6-6.86a1 1 0 0 0 0-1.7L9.52 4.29A1 1 0 0 0 8 5.14z',
    stop: 'M7 6h10a1 1 0 0 1 1 1v10a1 1 0 0 1-1 1H7a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1z',
    download: 'M19 9h-4V3H9v6H5l7 7 7-7zM5 18v2h14v-2H5z',
    update: 'M12 4V1L8 5l4 4V6c3.31 0 6 2.69 6 6 0 1.01-.25 1.97-.7 2.8l1.46 1.46A7.93 7.93 0 0 0 20 12c0-4.42-3.58-8-8-8zm0 14c-3.31 0-6-2.69-6-6 0-1.01.25-1.97.7-2.8L5.24 7.74A7.93 7.93 0 0 0 4 12c0 4.42 3.58 8 8 8v3l4-4-4-4v3z',
    pause: 'M7 5h3v14H7zM14 5h3v14h-3z',
    home: 'M10 20v-6h4v6h5v-8h3L12 3 2 12h3v8z',
    rooms: 'M4 4h7v7H4zM13 4h7v7h-7zM4 13h7v7H4zM13 13h7v7h-7z',
    people: 'M16 11c1.66 0 2.99-1.34 2.99-3S17.66 5 16 5c-1.66 0-3 1.34-3 3s1.34 3 3 3zm-8 0c1.66 0 2.99-1.34 2.99-3S9.66 5 8 5C6.34 5 5 6.34 5 8s1.34 3 3 3zm0 2c-2.33 0-7 1.17-7 3.5V19h14v-2.5c0-2.33-4.67-3.5-7-3.5zm8 0c-.29 0-.62.02-.97.05 1.16.84 1.97 1.97 1.97 3.45V19h6v-2.5c0-2.33-4.67-3.5-7-3.5z',
    feed: 'M21 19V5a2 2 0 0 0-2-2H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2zM8.5 13.5l2.5 3.01L14.5 12l4.5 6H5l3.5-4.5z',
    settings: 'M19.14 12.94c.04-.3.06-.61.06-.94s-.02-.64-.07-.94l2.03-1.58a.49.49 0 0 0 .12-.61l-1.92-3.32a.48.48 0 0 0-.59-.22l-2.39.96a7.03 7.03 0 0 0-1.62-.94l-.36-2.54a.48.48 0 0 0-.48-.41h-3.84a.47.47 0 0 0-.47.41l-.36 2.54c-.59.24-1.13.57-1.62.94l-2.39-.96a.48.48 0 0 0-.59.22L2.74 8.87a.48.48 0 0 0 .12.61l2.03 1.58c-.05.3-.09.63-.09.94s.02.64.07.94l-2.03 1.58a.49.49 0 0 0-.12.61l1.92 3.32c.12.22.37.29.59.22l2.39-.96c.5.38 1.03.7 1.62.94l.36 2.54c.05.24.24.41.48.41h3.84c.24 0 .44-.17.47-.41l.36-2.54c.59-.24 1.13-.56 1.62-.94l2.39.96c.22.08.47 0 .59-.22l1.92-3.32a.49.49 0 0 0-.12-.61l-2.01-1.58zM12 15.6A3.6 3.6 0 1 1 12 8.4a3.6 3.6 0 0 1 0 7.2z',
    open: 'M19 19H5V5h7V3H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14c1.1 0 2-.9 2-2v-7h-2v7zM14 3v2h3.59l-9.83 9.83 1.41 1.41L19 6.41V10h2V3h-7z',
    quit: 'M13 3h-2v10h2V3zm4.83 2.17-1.42 1.42A6.92 6.92 0 0 1 19 12c0 3.87-3.13 7-7 7A6.995 6.995 0 0 1 7.58 6.58L6.17 5.17A8.932 8.932 0 0 0 3 12a9 9 0 0 0 18 0c0-2.74-1.23-5.18-3.17-6.83z',
    bell: 'M12 22c1.1 0 2-.9 2-2h-4a2 2 0 0 0 2 2zm6-6v-5c0-3.07-1.64-5.64-4.5-6.32V4c0-.83-.67-1.5-1.5-1.5s-1.5.67-1.5 1.5v.68C7.63 5.36 6 7.92 6 11v5l-2 2v1h16v-1l-2-2z',
    chevron: 'M8.59 16.59 13.17 12 8.59 7.41 10 6l6 6-6 6z',
    star: 'M12 17.27 18.18 21l-1.64-7.03L22 9.24l-7.19-.61L12 2 9.19 8.63 2 9.24l5.46 4.73L5.82 21z',
    lock: 'M7.2 10.4V7.8a4.8 4.8 0 0 1 9.6 0v2.6h-2.4V7.8a2.4 2.4 0 0 0-4.8 0v2.6zM5 10.4h14a1 1 0 0 1 1 1V21a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1v-9.6a1 1 0 0 1 1-1z',
  };

  const SVG = 'http://www.w3.org/2000/svg';
  function icon(name, cls = '') {
    const svg = document.createElementNS(SVG, 'svg');
    svg.setAttribute('class', `ic ic-${name}${cls ? ` ${cls}` : ''}`);
    svg.setAttribute('viewBox', '0 0 24 24');
    svg.setAttribute('aria-hidden', 'true');
    svg.setAttribute('focusable', 'false');
    const path = document.createElementNS(SVG, 'path');
    path.setAttribute('d', PATHS[name]);
    svg.append(path);
    return svg;
  }

  const NETWORKS = [
    { id: 'receos', label: 'Rec EOS', logo: 'assets/receos-icon.png' },
    { id: 'radium', label: 'Radium', logo: 'logo.png' },
    { id: 'vanilla', label: 'Vanilla', logo: 'assets/vanilla-logo.png' },
    { id: 'stella', label: 'Stella', logo: 'assets/stella-logo.png' },
  ];

  /// "12:34", or "1:02:03" past the hour.
  function clock(since) {
    const s = Math.max(0, Math.floor((Date.now() - since) / 1000));
    const hh = Math.floor(s / 3600);
    const mm = Math.floor((s % 3600) / 60);
    const ss = String(s % 60).padStart(2, '0');
    return hh ? `${hh}:${String(mm).padStart(2, '0')}:${ss}` : `${mm}:${ss}`;
  }

  /// A picture the launcher named: only ever one of its own, or one through
  /// its thumbnail cache (which is all the window's CSP would load anyway).
  function picture(src) {
    const ok = typeof src === 'string'
      && /^(?:https?:\/\/radiumimg\.localhost\/|radiumimg:\/\/)/.test(src) && src.length < 4000;
    return ok ? src : 'assets/default-avatar.png';
  }

  // ── The panel ──────────────────────────────────────────────────────────

  /// The head: the network's own art, as Home's hero draws it — the same in
  /// every skin, because it is the brand, not the skin — with whether its
  /// servers are up and how many are playing.
  function hero(network, info, own) {
    const net = NETWORKS.find((n) => n.id === network);
    const game = info.game || {};
    const banner = own && bannerFor === network;
    const pills = [];
    if (game.state === 'stop') {
      pills.push(h('span', { class: 'pill is-playing' }, h('span', { class: 'dot' }),
        'Playing · ', h('span', { class: 'js-clock', 'data-since': game.startedAt || null },
          game.startedAt ? clock(game.startedAt) : '')));
    } else {
      const server = own ? info.server : 'checking';
      pills.push(h('span', { class: `pill is-${server === 'online' || server === 'offline' ? server : 'checking'}` },
        h('span', { class: 'dot' }),
        server === 'online' ? 'Servers online' : server === 'offline' ? 'Servers offline' : 'Checking servers…'));
    }
    const players = own ? info.players || {} : {};
    if (players.count && typeof players.count === 'string') {
      pills.push(h('span', { class: 'pill' }, icon('people'), `${players.count} online`));
    } else if (players.note) {
      pills.push(h('span', { class: 'pill is-quiet' }, players.note));
    }
    return h('div', { class: `hero${banner ? ' has-banner' : ''}`, 'data-key': 'hero', 'data-network': network },
      h('div', { class: 'hero-art', 'aria-hidden': 'true' }),
      h('div', { class: 'hero-brand' },
        h('span', { class: 'hero-mark', role: 'img', 'aria-label': net.label }),
        network === 'radium' ? null : h('span', { class: 'hero-name', 'aria-hidden': 'true' }, net.label)),
      h('div', { class: 'hero-pills' }, pills));
  }

  /// PLAY, or what stands in its place: STOP, DOWNLOAD, Stella's UPDATE, or
  /// a download's progress with Pause.
  function primary(network, info, own) {
    const game = own ? info.game || {} : {};
    const state = game.state || 'play';
    const wrap = (...kids) => h('div', { class: `primary is-${state}`, 'data-key': 'primary' }, kids);
    if (state === 'progress') {
      const pct = Number(game.pct);
      const known = Number.isFinite(pct) && pct >= 0;
      const fill = Math.max(0, Math.min(100, known ? pct : 0));
      return wrap(
        h('div', { class: 'progress' },
          h('div', { class: 'progress-top' },
            h('span', { class: 'progress-title' }, String(game.title || 'Downloading…')),
            h('span', { class: 'progress-pct' }, known ? `${Math.round(fill)}%` : '')),
          h('div', { class: `bar${known ? '' : ' indeterminate'}${game.paused ? ' is-paused' : ''}`,
                     role: 'progressbar', 'aria-valuemin': 0, 'aria-valuemax': 100,
                     'aria-valuenow': known ? Math.round(fill) : null },
            h('div', { class: 'bar-fill', style: `--pct: ${fill}` })),
          game.detail ? h('span', { class: 'progress-detail' }, String(game.detail)) : null),
        game.canPause
          ? h('button', { class: 'btn', type: 'button', 'data-id': game.paused ? 'dl:resume' : 'dl:pause' },
              icon(game.paused ? 'play' : 'pause'), game.paused ? 'Resume' : 'Pause')
          : null);
    }
    if (state === 'stop') {
      return wrap(h('button', { class: 'btn-pri is-run', type: 'button', 'data-id': 'play' }, icon('stop'), 'STOP'));
    }
    if (state === 'download') {
      return wrap(h('button', { class: 'btn-pri', type: 'button', 'data-id': 'download' },
        icon('download'), game.repair ? 'REPAIR' : 'DOWNLOAD'));
    }
    if (state === 'update') {
      return wrap(h('button', { class: 'btn-pri', type: 'button', 'data-id': 'dl:update' }, icon('update'), 'UPDATE'));
    }
    const mode = own && info.mode === 'vr' ? 'vr' : 'screen';
    return wrap(
      h('button', { class: 'btn-pri', type: 'button', 'data-id': 'play', disabled: state === 'starting' },
        icon('play'), state === 'starting' ? 'STARTING…' : 'PLAY'),
      seg({ class: 'seg-mode', 'aria-label': 'Play mode' },
        [{ id: 'mode:screen', content: 'Screen' }, { id: 'mode:vr', content: 'VR' }], `mode:${mode}`));
  }

  /// A segmented control: its options as radio buttons, and the thumb that
  /// marks the chosen one. The segments are all one width, so the thumb is
  /// placed by CSS from `--i` (which one) and `--n` (out of how many), and a
  /// change of choice — picked here or reported by the launcher — slides it
  /// across (see `.seg-thumb` in traymenu.css).
  ///
  /// The thumb carries its own copy of the labels, in the chosen colour and
  /// held in line with the real ones, so a label turns that colour exactly
  /// where the thumb covers it while it slides rather than all at once.
  function seg(attrs, options, chosen) {
    const at = Math.max(0, options.findIndex((o) => o.id === chosen));
    const copy = (content) => [content].flat().map((c) => (typeof c === 'string' ? c : c.cloneNode(true)));
    return h('div', { ...attrs, class: `seg ${attrs.class || ''}`.trim(), role: 'radiogroup',
                      style: `--i: ${at}; --n: ${options.length}` },
      h('span', { class: 'seg-thumb', 'aria-hidden': 'true', 'data-key': 'thumb' },
        h('span', { class: 'seg-thumb-row' },
          options.map((o) => h('span', { class: 'seg-label' }, copy(o.content))))),
      options.map((o) =>
        h('button', { class: 'seg-btn', type: 'button', role: 'radio', 'data-id': o.id,
                      'aria-checked': String(o.id === chosen) }, o.content)));
  }

  /// Stella's friends: up to three, online first, each with JOIN while they
  /// can be joined. Always as tall as three rows, so the panel doesn't change
  /// height as the list loads or friends come and go.
  function friends(info, own) {
    const data = own && info.friends && typeof info.friends === 'object' ? info.friends : { note: '' };
    const rows = Array.isArray(data.rows) ? data.rows.slice(0, 3) : [];
    const list = rows.length
      ? rows.map((f) => {
          const id = String(f.id || '');
          const status = ['online', 'offline'].includes(f.status) ? f.status : 'unknown';
          return h('div', { class: `friend is-${status}`, 'data-key': `friend:${id}` },
            h('button', { class: 'friend-main', type: 'button', 'data-id': `friend:${id}` },
              h('span', { class: 'avatar' },
                h('img', { src: picture(f.avatar), alt: '', draggable: 'false' }),
                h('span', { class: 'dot' })),
              h('span', { class: 'friend-text' },
                h('span', { class: 'friend-name' }, String(f.name || 'Player'),
                  f.favorite ? icon('star', 'friend-fav') : null),
                h('span', { class: 'friend-where' },
                  f.locked ? icon('lock', 'friend-lock') : null,
                  String(f.where || '')))),
            status === 'online'
              ? h('button', { class: 'btn-pri btn-small', type: 'button', 'data-id': `join:${id}`,
                              'aria-label': `Join ${f.name || 'your friend'}` }, 'JOIN')
              : null);
        })
      : [h('div', { class: 'friends-note' }, String(data.note || ''))];
    return h('section', { class: 'friends', 'data-key': 'friends', 'aria-label': 'Friends' },
      h('div', { class: 'sec-head' },
        h('span', { class: 'sec-title' }, 'Friends'),
        h('button', { class: 'sec-link', type: 'button', 'data-id': 'friends' },
          data.summary ? String(data.summary) : 'All friends', icon('chevron'))),
      h('div', { class: 'friend-list' }, list));
  }

  /// The system icon standing in for a line icon on the skins that have a set
  /// (`body[data-icons]`); traymenu.css shows one or the other.
  const sysIcon = (name) => h('span', { class: 'sys-ico', 'data-ico': name, 'aria-hidden': 'true' });

  function tiles(network) {
    const pages = [['home', 'Home'], ['rooms', 'Rooms'], ['people', 'People'],
                   // The Feed page exists on Vanilla only, as in the sidebar.
                   ...(network === 'vanilla' ? [['feed', 'Feed']] : []),
                   ['settings', 'Settings']];
    return h('nav', { class: 'tiles', 'data-key': 'tiles', 'aria-label': 'Pages' },
      pages.map(([id, label]) =>
        h('button', { class: 'tile has-sys', type: 'button', 'data-id': `tab:${id}`, 'data-key': `tab:${id}` },
          icon(id), sysIcon(id), h('span', {}, label))));
  }

  function networks(network) {
    return seg({ class: 'seg-networks', 'aria-label': 'Network', 'data-key': 'networks' },
      NETWORKS.map((n) => ({
        id: `network:${n.id}`,
        content: [h('img', { class: 'seg-logo', src: n.logo, alt: '', draggable: 'false' }), n.label],
      })), `network:${network}`);
  }

  function popups(on) {
    return h('button', { class: 'item item-switch has-sys', type: 'button', role: 'switch', 'data-key': 'popups',
                         'aria-checked': String(on), 'data-id': on ? 'popups:off' : 'popups:on' },
      icon('bell'), sysIcon('popups'), h('span', { class: 'item-label' }, 'Notification pop-ups'),
      h('span', { class: `switch${on ? ' on' : ''}`, 'aria-hidden': 'true' }, h('span', { class: 'knob' })));
  }

  function footer() {
    return h('div', { class: 'footer', 'data-key': 'footer' },
      h('button', { class: 'item item-open has-sys', type: 'button', 'data-id': 'open' },
        icon('open'), sysIcon('open'), h('span', { class: 'item-label' }, 'Open Rec EOS Launcher')),
      // No set has a shut-down icon, so Quit keeps its line icon everywhere.
      h('button', { class: 'item item-quit', type: 'button', 'data-id': 'quit' }, icon('quit'), 'Quit'));
  }

  const sep = (key) => h('div', { class: 'sep', role: 'separator', 'data-key': `sep:${key}` });

  /// The panel's sections, each keyed (`data-key`) so a redraw can tell which
  /// are the same section with new contents and which have come or gone.
  ///
  /// `network` is the one to draw, which for the measuring in `roomFor` is
  /// not always the one the launcher is on; the state's details belong to
  /// that one only.
  function build(state, network = state.network) {
    const active = NETWORKS.some((n) => n.id === network) ? network : 'radium';
    const own = active === state.network;
    const info = state.info && typeof state.info === 'object' ? state.info : {};
    // Whether Stella shows friends at all is Settings' call (Friends: Hidden),
    // and is known whichever network is on.
    const showFriends = active === 'stella' && (own ? !!info.friends : info.stellaFriends !== false);
    const popupsOn = own ? info.popups : true;
    return [
      hero(active, info, own),
      primary(active, info, own),
      ...(showFriends ? [sep('friends'), friends(info, own)] : []),
      sep('tiles'),
      tiles(active),
      sep('networks'),
      networks(active),
      sep('footer'),
      // Pop-ups are Vanilla's and Stella's notifications; Radium has none.
      ...(active !== 'radium' ? [popups(popupsOn !== false)] : []),
      footer(),
    ];
  }

  // ── Patching ───────────────────────────────────────────────────────────

  /// Make `live` look like `next`, keeping every element that is still there
  /// as the same element — so the button under the cursor keeps its hover
  /// and its focus, and a picture isn't fetched again — and only touching
  /// what differs. Children are matched in order, and by `data-key` where
  /// they have one. A child marked `data-transient` (the old head fading out
  /// over a network switch) isn't the panel's to patch, and is left to finish.
  function morph(live, next) {
    for (const name of live.getAttributeNames()) {
      if (!next.hasAttribute(name)) live.removeAttribute(name);
    }
    for (const name of next.getAttributeNames()) {
      const value = next.getAttribute(name);
      if (live.getAttribute(name) !== value) live.setAttribute(name, value);
    }
    const have = [...live.childNodes].filter((n) => !(n.nodeType === 1 && n.hasAttribute('data-transient')));
    const want = [...next.childNodes];
    want.forEach((node, i) => {
      const cur = have[i];
      if (!cur) {
        live.append(node);
      } else if (cur.nodeType !== node.nodeType || cur.nodeName !== node.nodeName
                 || (node.nodeType === 1 && cur.getAttribute('data-key') !== node.getAttribute('data-key'))) {
        cur.replaceWith(node);
      } else if (node.nodeType === 3) {
        if (cur.nodeValue !== node.nodeValue) cur.nodeValue = node.nodeValue;
      } else if (node.nodeType === 1) {
        morph(cur, node);
      }
    });
    for (const extra of have.slice(want.length)) extra.remove();
  }

  /// A section's box and opacity as drawn right now, in keyframe form —
  /// halfway through an animation if it is in one.
  function frame(el) {
    const cs = getComputedStyle(el);
    return {
      height: `${el.getBoundingClientRect().height}px`,
      paddingTop: cs.paddingTop,
      paddingBottom: cs.paddingBottom,
      marginTop: cs.marginTop,
      marginBottom: cs.marginBottom,
      opacity: cs.opacity,
    };
  }

  /// Bring an open panel's sections to `rows`, keeping every section that is
  /// still there (patched by `morph`) and inserting the new ones in place.
  ///
  /// Returns the sections whose size has to change, each with where it starts
  /// from: a new one from nothing, one that is going from its full size, and
  /// one caught mid-animation by a second network pick from wherever it had
  /// got to, so the panel carries on from there rather than jumping. Sections
  /// that are going stay in, marked `.leaving`, until the caller's `settle()`.
  function reconcile(rows) {
    const live = new Map([...menu.querySelectorAll(':scope > .moving')].map((el) => [el, frame(el)]));
    for (const el of menu.children) {
      for (const a of el.getAnimations()) a.cancel();
      el.classList.remove('moving');
    }
    const keys = new Set(rows.map((row) => row.dataset.key));
    const old = new Map([...menu.children].map((el) => [el.dataset.key, el]));
    const moves = [];
    let prev = null;
    for (const row of rows) {
      let el = old.get(row.dataset.key);
      if (el) {
        // Also takes `.leaving` off a section the new panel has back.
        morph(el, row);
        if (live.has(el)) moves.push({ el, from: live.get(el), to: 'open' });
      } else {
        el = row;
        moves.push({ el, from: null, to: 'open' });
        if (prev) prev.after(el);
        else menu.prepend(el);
      }
      prev = el;
    }
    for (const [key, el] of old) {
      if (keys.has(key)) continue;
      el.classList.add('leaving');
      moves.push({ el, from: live.get(el) || null, to: 'shut' });
    }
    return moves;
  }

  /// End a redraw: drop the sections that went, and let the rest be laid out
  /// as themselves again.
  function settle() {
    for (const el of menu.querySelectorAll(':scope > .leaving')) el.remove();
    for (const el of menu.children) {
      for (const a of el.getAnimations()) a.cancel();
      el.classList.remove('moving');
    }
  }

  /// Even in and out, so a section is seen opening the whole way. A curve
  /// that front-loads the motion (as 0.2, 0.8, 0.2, 1 did) opened the gap for
  /// a pop-ups row in its first frame, and the row then seemed to arrive late
  /// into a space already made for it.
  const EASE = 'cubic-bezier(0.4, 0, 0.2, 1)';

  /// Grow the new sections out of nothing and fold the departing ones away,
  /// so the panel's height glides to its new size and the sections below
  /// slide with it. Returned paused: the caller starts them once the window
  /// is big enough to hold the panel at its larger size.
  function animate(moves) {
    const shut = { height: '0px', paddingTop: '0px', paddingBottom: '0px', marginTop: '0px', marginBottom: '0px', opacity: '0' };
    const size = [];
    const fade = [];
    for (const { el, from, to } of moves) {
      // Measured at rest: nothing is animating it at this point.
      const full = { ...frame(el), opacity: '1' };
      const start = from || (to === 'open' ? shut : full);
      const end = to === 'open' ? full : shut;
      const box = (f) => ({ height: f.height, paddingTop: f.paddingTop, paddingBottom: f.paddingBottom,
                            marginTop: f.marginTop, marginBottom: f.marginBottom });
      el.classList.add('moving');
      // Coming in, the section opens and its contents fade up as it does,
      // a little behind, so they are fully there just as it is. Going out,
      // they fade first and the section folds a beat later: folded while
      // still visible, neighbouring sections' text squashes into one another.
      const opening = to === 'open';
      size.push(el.animate([box(start), box(end)], opening
        ? { duration: 240, easing: EASE }
        : { duration: 220, delay: from ? 0 : 40, easing: EASE, fill: 'both' }));
      fade.push(el.animate([{ opacity: start.opacity }, { opacity: end.opacity }], opening
        ? { duration: 190, delay: from ? 0 : 50, easing: 'ease-out', fill: 'backwards' }
        : { duration: 90, easing: 'ease-out', fill: 'forwards' }));
    }
    const all = [...size, ...fade];
    for (const a of all) a.pause();
    return {
      play: () => all.forEach((a) => a.play()),
      finished: Promise.all(size.map((a) => a.finished)),
    };
  }

  function motion() {
    return document.body.classList.contains('motion') && !matchMedia('(prefers-reduced-motion: reduce)').matches;
  }

  /// The old network's head — art, name and badges, as they were drawn —
  /// fading out over the new one, for a network picked while the panel is up.
  /// The whole of it, not only the art: with the art alone, the new name sat
  /// on the old network's art for most of the fade. The pictures cross, but
  /// the words go out before the new ones come in, so two names and two sets
  /// of badges are never read over each other.
  function crossfadeHero(from, parts) {
    const hero = menu.querySelector(':scope > .hero');
    if (!hero || !from || from === hero.dataset.network || !parts?.length) return;
    // Transient: the launcher's report, which usually lands mid-fade, patches
    // the head around it rather than removing it (see `morph`).
    const ghost = h('div', { class: 'hero-ghost', 'aria-hidden': 'true', 'data-network': from, 'data-transient': '' }, parts);
    hero.append(ghost);
    const words = ghost.querySelectorAll(':scope > :is(.hero-brand, .hero-pills)');
    for (const el of words) el.animate([{ opacity: 1 }, { opacity: 0 }], { duration: 110, easing: 'ease-in', fill: 'forwards' });
    ghost.animate([{ opacity: 1 }, { opacity: 0 }], { duration: 220, easing: 'ease-out', fill: 'forwards' })
      .finished.catch(() => {}).then(() => ghost.remove());
    for (const el of hero.querySelectorAll(':scope > :is(.hero-brand, .hero-pills)')) {
      el.animate([{ opacity: 0 }, { opacity: 1 }], { duration: 170, delay: 90, easing: 'ease-out', fill: 'backwards' });
    }
    setTimeout(() => ghost.remove(), 600);
  }

  // ── The user's own banner ──────────────────────────────────────────────

  /// The network whose Home banner (Settings → Home) is painted, if any.
  /// The launcher keeps it for its boot in localStorage, which this window
  /// shares; checked as boot.js checks it, a stored picture only.
  let bannerFor = '';
  function loadBanner(network) {
    let image = '';
    try {
      const banner = JSON.parse(localStorage.getItem('radium-home-banner') || 'null');
      if (banner && banner.network === network) image = String(banner.image || '');
    } catch (e) { /* no banner */ }
    if (image.startsWith('data:image/') && !/['"(){}\\]|[\x00-\x1f\x7f]/.test(image)) {
      root.setProperty('--tm-banner', `url("${image}")`);
      bannerFor = network;
    } else {
      root.removeProperty('--tm-banner');
      bannerFor = '';
    }
  }

  // ── Showing ────────────────────────────────────────────────────────────

  let open = false;

  /// Settle the webfont before anything is measured.
  ///
  /// The launcher's menus are set in Inter under the modern skins, so that is
  /// what `--tm-font` asks for — and a webfont only starts loading when
  /// something uses it, which is `build()` above. Measuring straight after it
  /// therefore measured the fallback face: 317px tall against Inter's 308px
  /// for the same menu, so the very first menu of a session was sized 9px too
  /// tall and then reflowed inside a window that no longer fitted it. Every
  /// menu after that measured correctly, because by then the font was loaded —
  /// which is exactly why only the first one opened differently.
  ///
  /// Resolved immediately once the font is in, so this costs nothing after the
  /// first menu. The race is there so a font that never arrives cannot leave
  /// the menu invisible.
  ///
  /// The layout read is not redundant, and `document.fonts.status` cannot
  /// stand in for it: a webfont is only requested when something actually
  /// needs it, so before the menu has been laid out nothing is pending and the
  /// status reads "loaded" — the one value that looks like "nothing to wait
  /// for". Forcing layout here is what puts the face in flight, and only then
  /// does `ready` mean what it says.
  function fontsSettled() {
    if (!document.fonts) return Promise.resolve();
    menu.getBoundingClientRect();
    if (document.fonts.status === 'loaded') return Promise.resolve();
    return Promise.race([
      document.fonts.ready,
      new Promise((done) => setTimeout(done, 400)),
    ]);
  }

  /// Hand the backend the panel's size so it can place the window, and return
  /// the frost it captured (Liquid Glass only — see frost.rs).
  ///
  /// The window is sized by `roomFor`, not by the panel on show, so switching
  /// network never has to resize it. `tray_menu_show` places the window by
  /// the stored cursor anchor, so calling it again while the panel is up
  /// resizes it in place — which only a skin change, or a download starting
  /// under it, needs (`resize`).
  async function place(state, { resize = false } = {}) {
    await fontsSettled();
    const { width, height } = roomFor(state);
    if (resize && placedSize?.width >= width && placedSize?.height >= height
        && placedSize.width - width < 2 && placedSize.height - height < 2) return null;
    const frost = document.body.classList.contains('glass');
    const placed = await invoke('tray_menu_show', { width, height, frost });
    placedSize = { width, height };
    document.body.classList.toggle('above', placed?.above !== false);
    return placed?.frost;
  }

  /// The window's size as last placed, in CSS pixels.
  let placedSize = null;

  const probe = menu.cloneNode(false);
  probe.removeAttribute('id');
  probe.removeAttribute('role');
  probe.removeAttribute('aria-label');
  probe.setAttribute('aria-hidden', 'true');
  probe.classList.add('probe');
  document.body.appendChild(probe);

  /// Room for the panel whichever network is picked: the largest of the
  /// networks' panels, measured in `probe` under the current skin. Stella's
  /// has friends and the others don't, so the panel's height changes as you
  /// switch, but the window doesn't have to. Resizing it with the panel
  /// flickered: Windows moves and sizes the window a frame before the
  /// webview draws to the new size.
  function roomFor(state) {
    let width = 0;
    let height = 0;
    for (const { id } of NETWORKS) {
      probe.replaceChildren(...build(state, id));
      const rect = probe.getBoundingClientRect();
      width = Math.max(width, Math.ceil(rect.width));
      height = Math.max(height, Math.ceil(rect.height));
    }
    probe.replaceChildren();
    return { width, height };
  }

  /// Bumped by every redraw, so an animation that a newer one has overtaken
  /// leaves the sections to it.
  let generation = 0;

  /// The state the panel was last drawn from, for drawing a network picked
  /// here ahead of the launcher's answer (`foresee`).
  let drawn = null;

  /// The panel on `network`, as far as it can be known before the launcher
  /// has switched: its art and name, its pages, whether it has friends and
  /// pop-ups — all of which depend on the network alone — with its servers
  /// and players still to be checked. PLAY keeps its state until the
  /// launcher reports the new network's. The launcher's report, a moment
  /// later, then only fills in; and a switch it refuses puts the old one back.
  function foresee(state, network) {
    const info = state.info && typeof state.info === 'object' ? state.info : {};
    const friends = network === 'stella' && info.stellaFriends !== false ? { note: 'Loading friends…' } : null;
    return { ...state, network, info: { ...info, server: 'checking', players: {}, friends } };
  }

  /// The play clock and anything else that reads the time, ticked while the
  /// panel is up.
  let ticker = null;
  function tick() {
    for (const el of menu.querySelectorAll('.js-clock[data-since]')) {
      const since = Number(el.dataset.since);
      if (since > 0) {
        const text = clock(since);
        if (el.textContent !== text) el.textContent = text;
      }
    }
  }

  async function show(state) {
    if (!state) return;
    generation++;
    drawn = state;
    // A pick from the last time the panel was up is long settled.
    clearTimeout(picked?.timer);
    picked = null;
    applyStyle(state.style);
    loadBanner(state.network);
    menu.replaceChildren(...build(state));
    menu.classList.remove('in');
    open = true;
    clearInterval(ticker);
    ticker = setInterval(tick, 1000);
    root.removeProperty('--tm-backdrop');
    try {
      const url = await place(state);
      // The panel fades in once it has its frost.
      //
      // Deliberately not deferred to a `requestAnimationFrame` first. Waiting
      // a frame here looks like the careful thing to do — start the animation
      // once the window is really on screen — but rAF does not reliably fire
      // in a window that has only just been shown, and nothing is what this
      // class guards: `.menu` is `opacity: 0` until it arrives, so a callback
      // that never runs is a panel that never appears. The cold-renderer
      // problem it was meant to cover is handled where it belongs, by giving
      // the webview a frame at startup (`warm_tray_menu` in background.rs)
      // and painting a panel once off screen (`warm` below).
      if (url) root.setProperty('--tm-backdrop', `url("${url}")`);
      if (open) menu.classList.add('in');
    } catch (e) {
      open = false;
    }
  }

  /// Paint a real panel once with the window parked off screen, entrance and
  /// all, so the first right-click of a session isn't the first time any of
  /// it is drawn (see "Painting the panel once" in background.rs). Every
  /// network's art goes in, so the first network switch finds it decoded too.
  /// The backend hides the window again and says so (`tray-menu-warm-done`).
  async function warm(state) {
    if (open || !state) return;
    applyStyle(state.style);
    loadBanner(state.network);
    menu.replaceChildren(...build(state));
    const hero = menu.querySelector(':scope > .hero');
    for (const { id } of NETWORKS) {
      if (id !== state.network) {
        hero?.append(h('div', { class: 'hero-ghost', 'aria-hidden': 'true', 'data-network': id },
          h('div', { class: 'hero-art' })));
      }
    }
    menu.classList.remove('in');
    try {
      await fontsSettled();
      if (open) return;
      const { width, height } = roomFor(state);
      await invoke('tray_menu_warm', { width, height });
      if (!open) menu.classList.add('in');
    } catch (e) { /* the first open simply paints it, as it always did */ }
  }

  function warmDone() {
    if (open) return;
    menu.classList.remove('in');
    for (const ghost of menu.querySelectorAll('.hero-ghost')) ghost.remove();
  }

  /// Redraw an open panel after the launcher's state changed under it — a
  /// network picked here, a download's progress, a friend coming online. No
  /// entrance animation and no new frost: the panel is already on screen and
  /// stays put, only its contents and its height change.
  ///
  /// When sections come or go (only Stella has friends, Radium has no
  /// pop-ups), the height change is animated, entirely inside the window: it
  /// already has room for every network's panel (`roomFor`), and the panel is
  /// pinned to its edge by the cursor, so the rest of it is just clear.
  async function update(state) {
    if (!state || !open) return;
    const gen = ++generation;
    drawn = state;
    applyStyle(state.style);
    const head = menu.querySelector(':scope > .hero');
    const was = head?.dataset.network;
    // The head as it is now, to fade out over the new one (crossfadeHero).
    const parts = head && was !== state.network && motion()
      ? [...head.children].filter((c) => !c.classList.contains('hero-ghost')).map((c) => c.cloneNode(true))
      : null;
    if (was !== state.network) loadBanner(state.network);
    const moves = reconcile(build(state));
    crossfadeHero(was, parts);
    // Started from their first frame now, before anything awaits, so the
    // sections' new layout is never drawn ahead of the animation.
    const run = moves.length && motion() ? animate(moves) : null;
    if (!run) settle();
    try {
      // A skin change can make every panel bigger or smaller; a network
      // switch doesn't change the room needed, and makes no call.
      await place(state, { resize: true });
      if (!run || gen !== generation) return;
      run.play();
      // A backstop for the frames, as in `close`: finished sections must not
      // be left in the panel if this window stops ticking animations.
      await Promise.race([run.finished, new Promise((done) => setTimeout(done, 600))]);
      if (gen !== generation) return;
      settle();
    } catch (e) {
      /* The panel is still up and still correct; only its size may lag. */
      if (gen === generation) settle();
    }
  }

  /// Blank the panel, let that frame reach the screen, then hide the window,
  /// so the next time it is shown Windows doesn't flash this panel first.
  ///
  /// The timer is a backstop for the frames. `requestAnimationFrame` does not
  /// always run in this window (see `show`), and if it didn't here, a blanked
  /// but still-shown window would sit on top of everything by the taskbar,
  /// invisible and catching clicks. Whichever comes first hides it; neither
  /// does if the panel has been opened again in the meantime.
  function close() {
    if (!open) return;
    open = false;
    clearInterval(ticker);
    menu.classList.remove('in');
    let done = false;
    const hide = () => {
      if (done || open) return;
      done = true;
      invoke('tray_menu_hide').catch(() => {});
    };
    requestAnimationFrame(() => requestAnimationFrame(hide));
    setTimeout(hide, 150);
  }

  // ── Choosing ───────────────────────────────────────────────────────────

  /// Choices that are settings rather than commands: the panel stays up, as a
  /// radio group or a switch in a menu does, and shows the change at once
  /// rather than after the round trip. The launcher then reports back, and a
  /// change it refused is put back by that update. Mirrors keeps_menu_open in
  /// background.rs.
  const STAYS_OPEN = ['network:', 'mode:', 'popups:', 'dl:'];

  function pickInPlace(btn, id) {
    if (btn.getAttribute('role') === 'radio') {
      if (btn.getAttribute('aria-checked') === 'true') return false;
      const radios = [...btn.parentElement.querySelectorAll('[role="radio"]')];
      for (const other of radios) {
        other.setAttribute('aria-checked', String(other === btn));
      }
      // The thumb starts sliding now, not when the launcher answers.
      btn.parentElement.style.setProperty('--i', String(radios.indexOf(btn)));
    } else if (btn.getAttribute('role') === 'switch') {
      const on = id === 'popups:on';
      btn.setAttribute('aria-checked', String(on));
      btn.dataset.id = on ? 'popups:off' : 'popups:on';
      btn.querySelector('.switch')?.classList.toggle('on', on);
    }
    return true;
  }

  menu.addEventListener('click', (e) => {
    const btn = e.target.closest('button[data-id]');
    if (!btn || !open || btn.disabled || btn.closest('.leaving')) return;
    const id = btn.dataset.id;
    if (STAYS_OPEN.some((prefix) => id.startsWith(prefix))) {
      if (!pickInPlace(btn, id)) return;
      invoke('tray_menu_pick', { id }).catch(() => {});
      // A network picked: the panel changes over now, not when the launcher
      // has finished switching (which took 30-90 ms, the sections and art
      // starting only once the highlight was already well on its way).
      if (id.startsWith('network:') && drawn) {
        const network = id.slice('network:'.length);
        clearTimeout(picked?.timer);
        picked = { network, held: null };
        picked.timer = setTimeout(() => {
          const late = picked?.held;
          picked = null;
          if (late) update(late);
        }, 800);
        update(foresee(drawn, network));
      }
      return;
    }
    open = false;
    clearInterval(ticker);
    menu.classList.remove('in');
    invoke('tray_menu_pick', { id }).catch(() => {});
  });

  // Arrow keys walk the panel's buttons in reading order, as a menu's do;
  // Tab works too.
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') {
      close();
      return;
    }
    const step = { ArrowDown: 1, ArrowRight: 1, ArrowUp: -1, ArrowLeft: -1 }[e.key];
    if (!step) return;
    e.preventDefault();
    const items = [...menu.querySelectorAll('button:not(:disabled)')].filter((b) => !b.closest('.leaving'));
    if (!items.length) return;
    const at = items.indexOf(document.activeElement);
    const next = at < 0 ? (step > 0 ? 0 : items.length - 1) : (at + step + items.length) % items.length;
    items[next].focus();
  });

  document.addEventListener('contextmenu', (e) => e.preventDefault());

  // A friend's picture that didn't load shows the default one.
  document.addEventListener('error', (e) => {
    const img = e.target;
    if (img?.tagName === 'IMG' && img.closest('.avatar') && !img.src.endsWith('assets/default-avatar.png')) {
      img.src = 'assets/default-avatar.png';
    }
  }, true);

  // The window is taller than a shorter network's panel (see `roomFor`), and
  // that clear strip is still this window. A click there is a click outside
  // the panel, so it closes it, as a click anywhere else would.
  document.addEventListener('mousedown', (e) => {
    if (!e.target.closest?.('.menu')) close();
  });

  listen('tray-menu-open', (event) => show(event.payload));
  /// A network just picked here, while the launcher switches to it. A report
  /// it sent before it had switched (still the old network) would flick the
  /// panel back for a frame, so one is held — and drawn after all only if the
  /// switch never comes.
  let picked = null;

  listen('tray-menu-update', (event) => {
    const state = event.payload;
    if (picked && state?.network !== picked.network) {
      picked.held = state;
      return;
    }
    if (picked) {
      clearTimeout(picked.timer);
      picked = null;
    }
    update(state);
  });
  listen('tray-menu-dismiss', close);
  listen('tray-menu-warm', (event) => warm(event.payload));
  listen('tray-menu-warm-done', warmDone);

  // Fetch the panel's face now rather than when the first panel is measured.
  // This page is normally built well before any right-click (see
  // `warm_tray_menu`), so by the time a panel is drawn the font is already
  // in and `fontsSettled()` has nothing to wait for. traymenu.css declares one
  // webface over the whole weight range, so this is all of it.
  if (document.fonts) {
    document.fonts.load('400 12px Inter').catch(() => {});
    document.fonts.load('700 12px Inter').catch(() => {});
  }

  // A right-click that came before this page finished loading.
  invoke('tray_menu_state').then(show).catch(() => {});
})();
