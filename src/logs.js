// ─── Logs page and Report a Problem ──────────────────────────────────────────
// The log itself is kept by addLog() at the top of app.js (in memory, and on
// disk through applog.rs); this file draws it on the Logs page and files bug
// reports from it. Loaded after app.js, whose helpers ($, toast, showModal,
// hideModal) and state (isInstalled, isGameRunning, ...) it uses.

/// Which LOG_LEVELS each level filter shows. "Info" takes OK lines too: they
/// are good news, not a level anyone filters for on its own.
const LOG_FILTERS = {
  all:   null,
  info:  ['info', 'ok'],
  warn:  ['warn'],
  error: ['error'],
};

/// Source labels as the filter menu and rows show them.
const LOG_SOURCE_NAMES = {
  launcher: 'Launcher', update: 'Updates', server: 'Servers',
  account: 'Account', install: 'Install', game: 'Game',
};

/// Parse a session file back into entries: the same shape addLog() keeps,
/// plus `marker` rows for the "===== Session started =====" lines. Lines a
/// message wrapped onto are indented under it (see logLineText()).
function parseLogText(text) {
  const entries = [];
  const head = /^\[(\d\d:\d\d:\d\d|--:--:--)\] (INFO|OK|WARN|ERROR|PANIC)\s+([A-Z]+)\s{2}(.*)$/;
  const levels = { INFO: 'info', OK: 'ok', WARN: 'warn', ERROR: 'error', PANIC: 'error' };
  const sources = Object.fromEntries(Object.entries(LOG_SOURCES).map(([k, v]) => [v, k]));
  let seq = 0;
  for (const raw of text.split(/\r?\n/)) {
    if (!raw.trim()) continue;
    const m = raw.match(head);
    if (m) {
      entries.push({
        seq: ++seq, time: m[1], level: levels[m[2]], source: sources[m[3]] || 'launcher',
        msg: m[2] === 'PANIC' ? `Crashed: ${m[4]}` : m[4], line: raw,
      });
    } else if (/^=====.*=====$/.test(raw.trim())) {
      entries.push({ seq: ++seq, marker: raw.trim().replace(/^=+\s*|\s*=+$/g, '') });
    } else if (entries.length && !entries[entries.length - 1].marker) {
      const last = entries[entries.length - 1];
      last.msg += '\n' + raw.trim();
      last.line += '\n' + raw;
    } else {
      entries.push({ seq: ++seq, time: '', level: 'info', source: 'launcher', msg: raw.trim(), line: raw });
    }
  }
  return entries;
}

/// Copy text to the clipboard, falling back to the old selection trick where
/// the async API is refused.
async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch (e) {
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.style.cssText = 'position:fixed;left:-9999px;top:0';
    document.body.appendChild(ta);
    ta.select();
    let ok = false;
    try { ok = document.execCommand('copy'); } catch (e2) {}
    ta.remove();
    return ok;
  }
}

// ── Sliding highlight for the segmented controls ─────────────────────────
// Each .log-seg gets a thumb that glides to the selected button, like the
// play-mode control. Only Liquid Glass draws it; every skin keeps its instant
// chip fill (the user found the glide wrong on the modern skins).

function placeSegThumb(seg, animate = true) {
  let thumb = seg.querySelector(':scope > .log-seg-thumb');
  if (!thumb) {
    thumb = document.createElement('span');
    thumb.className = 'log-seg-thumb';
    thumb.setAttribute('aria-hidden', 'true');
    seg.prepend(thumb);
  }
  const active = seg.querySelector(':scope > .log-seg-btn.active');
  // Not laid out (a hidden tab or dialog): placed again once it shows.
  if (!active || !active.offsetWidth) { thumb.style.opacity = '0'; return; }
  thumb.classList.toggle('no-anim', !animate || thumb.style.opacity === '0');
  thumb.style.opacity = '';
  thumb.style.width = `${active.offsetWidth}px`;
  thumb.style.transform = `translateX(${active.offsetLeft}px)`;
  if (thumb.classList.contains('no-anim')) {
    void thumb.offsetWidth;              // commit the jump before re-enabling
    thumb.classList.remove('no-anim');
  }
}

function placeSegThumbs(root = document, animate = false) {
  root.querySelectorAll('.log-seg').forEach(seg => placeSegThumb(seg, animate));
}

document.querySelectorAll('.log-seg').forEach(seg => {
  // Follows the selection however it changes, and the buttons' sizes (a
  // count going from 9 to 10 widens its chip).
  new MutationObserver((records) => {
    // The thumb's own class changes are not a new selection.
    if (records.some(r => !r.target.classList.contains('log-seg-thumb'))) placeSegThumb(seg);
  }).observe(seg, { subtree: true, attributes: true, attributeFilter: ['class'] });
  const resize = new ResizeObserver(() => placeSegThumb(seg));
  seg.querySelectorAll('.log-seg-btn').forEach(b => resize.observe(b));
  placeSegThumb(seg, false);
});

// ── The Logs page ────────────────────────────────────────────────────────

const logView = (() => {
  const panel = $('tab-status');
  const out = $('logOutput');
  const list = $('logList');
  if (!panel || !out || !list) return null;

  const state = {
    level: 'all',
    source: '',
    query: '',
    session: 'current',   // or 'previous'
    clearedSeq: 0,        // Clear hides everything up to here
  };
  /// The last session, once asked for: { entries, ending } or false for none.
  let previous = null;
  /// New matching lines that arrived while the reader was scrolled up.
  let unseen = 0;
  /// Something changed while the page was hidden; redraw when it shows.
  let stale = true;
  /// The row whose details are open.
  let openRow = null;
  /// When Clear was pressed.
  let clearedAt = '';

  const visible = () => panel.classList.contains('active');

  function sessionEntries() {
    if (state.session === 'previous') return previous ? previous.entries : [];
    // Opened by a divider naming when the session started (or was cleared),
    // the way a session read back from its file opens.
    const head = state.clearedSeq
      ? { seq: 0, marker: `Cleared at ${clearedAt}` }
      : { seq: 0, marker: `Session started ${logDate(logSessionStart)} ${logClock(logSessionStart)}` };
    return [head, ...logEntries.filter(e => e.seq > state.clearedSeq)];
  }

  function matchesText(e) {
    if (state.source && e.source !== state.source) return false;
    if (!state.query) return true;
    const q = state.query.toLowerCase();
    return e.msg.toLowerCase().includes(q) || LOG_SOURCES[e.source].toLowerCase().includes(q);
  }
  function matches(e) {
    if (e.marker) return !state.query && !state.source && state.level === 'all';
    const levels = LOG_FILTERS[state.level];
    return (!levels || levels.includes(e.level)) && matchesText(e);
  }

  /// The message with every hit of the search wrapped in <mark>.
  function messageNodes(msg) {
    const frag = document.createDocumentFragment();
    if (!state.query) { frag.append(msg); return frag; }
    const lower = msg.toLowerCase(), q = state.query.toLowerCase();
    let at = 0;
    for (let i = lower.indexOf(q); i !== -1; i = lower.indexOf(q, at)) {
      if (i > at) frag.append(msg.slice(at, i));
      const mark = document.createElement('mark');
      mark.className = 'log-hit';
      mark.textContent = msg.slice(i, i + q.length);
      frag.append(mark);
      at = i + q.length;
    }
    if (at < msg.length) frag.append(msg.slice(at));
    return frag;
  }

  function buildRow(e) {
    if (e.marker) {
      const m = document.createElement('div');
      m.className = 'log-marker';
      m.dataset.seq = e.seq;
      m.textContent = e.marker;
      return m;
    }
    const row = document.createElement('div');
    row.className = `log-row lvl-${e.level}`;
    row.dataset.seq = e.seq;
    row.tabIndex = -1;

    const ts = document.createElement('span');
    ts.className = 'log-ts';
    ts.textContent = e.time;
    const lvl = document.createElement('span');
    lvl.className = 'log-lvl';
    lvl.textContent = LOG_LEVELS[e.level];
    const src = document.createElement('span');
    src.className = 'log-src';
    src.textContent = LOG_SOURCES[e.source];
    const msg = document.createElement('span');
    msg.className = 'log-msg';
    msg.append(messageNodes(e.msg));
    const more = document.createElement('span');
    more.className = 'log-more';
    more.setAttribute('aria-hidden', 'true');

    row.append(ts, lvl, src, msg, more);
    if (e.msg.includes('\n')) row.classList.add('is-long');
    return row;
  }

  /// Rows whose message runs past its column get the chevron and open on a
  /// click. One read pass after the writes, so the layout is worked out once.
  function markLongRows(rows) {
    const long = [];
    for (const row of rows) {
      const msg = row.querySelector('.log-msg');
      if (msg && msg.scrollWidth > msg.clientWidth + 1) long.push(row);
    }
    for (const row of long) row.classList.add('is-long');
  }

  function nearBottom() {
    return out.scrollHeight - out.scrollTop - out.clientHeight < 28;
  }
  function toBottom() {
    out.scrollTop = out.scrollHeight;
    unseen = 0;
    showJump();
  }

  function showJump() {
    const jump = $('logJump');
    if (!jump) return;
    jump.hidden = unseen === 0;
    $('logJumpText').textContent = unseen === 1 ? '1 new line' : `${unseen} new lines`;
  }

  function updateCounts() {
    const counts = { all: 0, info: 0, warn: 0, error: 0 };
    for (const e of sessionEntries()) {
      if (e.marker || !matchesText(e)) continue;
      counts.all++;
      if (e.level === 'warn') counts.warn++;
      else if (e.level === 'error') counts.error++;
      else counts.info++;
    }
    panel.querySelectorAll('[data-count]').forEach(el => {
      el.textContent = counts[el.dataset.count];
    });
    // The Errors chip reads as an alert only while it has something in it.
    panel.querySelector('.log-level[data-level="error"]')?.classList.toggle('has-errors', counts.error > 0);
  }

  function updateEmpty(shown) {
    const empty = $('logEmpty');
    empty.hidden = shown > 0;
    if (shown > 0) return;
    const filtering = state.query || state.source || state.level !== 'all';
    const cleared = state.session === 'current' && state.clearedSeq > 0;
    $('logEmptyTitle').textContent = filtering ? 'No lines match'
      : state.session === 'previous' ? 'The last session logged nothing'
      : cleared ? 'Cleared' : 'Nothing logged yet';
    $('logEmptyText').textContent = filtering ? 'Try another filter, or clear the search.'
      : state.session === 'previous' ? ''
      : cleared ? 'New lines will appear here.' : 'Lines appear here as the launcher works.';
  }

  function updateStatus() {
    const info = $('logSessionInfo');
    const all = sessionEntries().filter(e => !e.marker);
    const errors = all.filter(e => e.level === 'error').length;
    const lines = `${all.length} ${all.length === 1 ? 'line' : 'lines'}`;
    const errText = errors ? ` · ${errors} ${errors === 1 ? 'error' : 'errors'}` : '';
    if (state.session === 'previous') {
      info.textContent = previous ? `${lines}${errText} · ${previous.endingText}` : '';
      info.classList.toggle('is-bad', !!previous && previous.ending !== 'closed');
    } else {
      info.textContent = `${lines}${errText}`;
      info.classList.remove('is-bad');
    }
    $('btnLogClear').disabled = state.session !== 'current' || all.length === 0;
    $('btnLogCopy').disabled = $('btnLogSave').disabled = all.length === 0;
  }

  function render() {
    stale = false;
    openRow = null;
    const frag = document.createDocumentFragment();
    const rows = [];
    const shown = sessionEntries().filter(matches);
    const lines = shown.filter(e => !e.marker).length;
    // A divider over no lines at all says nothing; the empty state does.
    for (const e of lines ? shown : []) {
      const row = buildRow(e);
      rows.push(row);
      frag.appendChild(row);
    }
    list.replaceChildren(frag);
    markLongRows(rows);
    updateEmpty(lines);
    updateCounts();
    updateStatus();
    toBottom();
  }

  // A line logged while the page is open is added in place; otherwise the
  // page is redrawn the next time it is shown.
  logListeners.push((e) => {
    if (state.session !== 'current') return;
    if (!visible()) { stale = true; return; }
    updateCounts();
    updateStatus();
    if (!matches(e)) return;
    // The first line under an empty state brings its divider with it.
    if (!list.firstElementChild) { render(); return; }
    const stick = nearBottom();
    const row = buildRow(e);
    list.appendChild(row);
    markLongRows([row]);
    // The page holds LOG_MAX lines; drop the rows that have fallen off it.
    while (list.firstElementChild && Number(list.firstElementChild.dataset.seq) <= logSeq - LOG_MAX) {
      list.firstElementChild.remove();
    }
    updateEmpty(1);
    if (stick) toBottom();
    else { unseen++; showJump(); }
  });

  out.addEventListener('scroll', () => {
    if (unseen && nearBottom()) { unseen = 0; showJump(); }
  }, { passive: true });
  $('logJump')?.addEventListener('click', toBottom);

  new MutationObserver(() => {
    if (!visible()) return;
    if (stale) render();
    checkPrevious();
  }).observe(panel, { attributes: true, attributeFilter: ['class'] });

  // Re-measure which rows are cut off when the window is resized.
  let resizeTimer = null;
  new ResizeObserver(() => {
    clearTimeout(resizeTimer);
    resizeTimer = setTimeout(() => {
      const rows = [...list.querySelectorAll('.log-row')];
      rows.forEach(r => { if (!r.querySelector('.log-msg').textContent.includes('\n')) r.classList.remove('is-long'); });
      markLongRows(rows);
    }, 150);
  }).observe(out);

  // ── Row details ──
  function entryFor(row) {
    const seq = Number(row.dataset.seq);
    return sessionEntries().find(e => e.seq === seq);
  }
  function lineOf(e) { return e.line || logLineText(e); }

  function closeDetails() {
    if (!openRow) return;
    openRow.classList.remove('is-open');
    openRow.querySelector('.log-row-actions')?.remove();
    openRow = null;
  }
  function openDetails(row) {
    const e = entryFor(row);
    if (!e) return;
    closeDetails();
    openRow = row;
    row.classList.add('is-open');
    const actions = document.createElement('div');
    actions.className = 'log-row-actions';
    const copy = document.createElement('button');
    copy.type = 'button';
    copy.className = 'log-row-btn';
    copy.textContent = 'Copy line';
    copy.addEventListener('click', async (ev) => {
      ev.stopPropagation();
      toast(await copyText(lineOf(e)) ? 'Line copied.' : "Couldn't copy the line.", 'info', 2000);
    });
    actions.append(copy);
    if (e.level === 'error' || e.level === 'warn') {
      const rep = document.createElement('button');
      rep.type = 'button';
      rep.className = 'log-row-btn is-report';
      rep.textContent = 'Report this';
      rep.addEventListener('click', (ev) => {
        ev.stopPropagation();
        openReport({ linked: e });
      });
      actions.append(rep);
    }
    row.append(actions);
  }
  list.addEventListener('click', (ev) => {
    if (ev.target.closest('.log-row-actions')) return;
    const row = ev.target.closest('.log-row');
    if (!row) return;
    // Selecting text inside a row is not a click on it.
    if (String(window.getSelection?.() || '').length) return;
    if (row === openRow) { closeDetails(); return; }
    if (row.classList.contains('is-long') || row.matches('.lvl-error, .lvl-warn')) openDetails(row);
  });

  // ── Toolbar ──
  panel.querySelectorAll('.log-level').forEach(btn => {
    btn.addEventListener('click', () => {
      state.level = btn.dataset.level;
      panel.querySelectorAll('.log-level').forEach(b => {
        const on = b === btn;
        b.classList.toggle('active', on);
        b.setAttribute('aria-checked', on ? 'true' : 'false');
      });
      render();
    });
  });
  $('logSource')?.addEventListener('change', (ev) => {
    state.source = ev.target.value;
    render();
  });

  const search = $('logSearch');
  let searchTimer = null;
  search?.addEventListener('input', () => {
    $('logSearchClear').hidden = !search.value;
    clearTimeout(searchTimer);
    searchTimer = setTimeout(() => {
      state.query = search.value.trim();
      render();
    }, 120);
  });
  search?.addEventListener('keydown', (ev) => {
    if (ev.key === 'Escape' && search.value) {
      ev.stopPropagation();
      $('logSearchClear').click();
    }
  });
  $('logSearchClear')?.addEventListener('click', () => {
    search.value = '';
    $('logSearchClear').hidden = true;
    state.query = '';
    render();
    search.focus();
  });
  // Ctrl+F on the Logs page goes to its search box.
  document.addEventListener('keydown', (ev) => {
    if ((ev.ctrlKey || ev.metaKey) && ev.key.toLowerCase() === 'f' && visible()
        && !document.querySelector('.modal-overlay[style*="flex"]')) {
      ev.preventDefault();
      search?.focus();
      search?.select();
    }
  });

  // ── Sessions ──
  async function loadPrevious() {
    if (previous !== null) return previous;
    try {
      const prev = await window.radium?.logPrevious();
      previous = prev ? {
        entries: parseLogText(prev.text),
        ending: prev.ending,
        endingText: { closed: 'closed normally', crashed: 'crashed', unexpected: "didn't close normally" }[prev.ending] || '',
      } : false;
    } catch (e) {
      previous = false;
    }
    return previous;
  }
  /// Whether there is a last session to show, asked once the page is opened.
  async function checkPrevious() {
    const btn = panel.querySelector('.log-session-btn[data-session="previous"]');
    if (!btn || btn.dataset.checked) return;
    btn.dataset.checked = '1';
    const prev = await loadPrevious();
    btn.disabled = !prev;
    if (prev && prev.ending !== 'closed') btn.classList.add('is-bad');
  }
  panel.querySelectorAll('.log-session-btn').forEach(btn => {
    btn.addEventListener('click', async () => {
      if (btn.dataset.session === 'previous' && !(await loadPrevious())) return;
      state.session = btn.dataset.session;
      panel.querySelectorAll('.log-session-btn').forEach(b => {
        const on = b === btn;
        b.classList.toggle('active', on);
        b.setAttribute('aria-checked', on ? 'true' : 'false');
      });
      panel.classList.toggle('viewing-previous', state.session === 'previous');
      render();
    });
  });

  // ── Actions ──
  function shownText() {
    return sessionEntries().filter(matches).map(e => e.marker ? `===== ${e.marker} =====` : lineOf(e)).join('\n');
  }
  function shownCount() {
    return sessionEntries().filter(e => !e.marker && matches(e)).length;
  }
  $('btnLogCopy')?.addEventListener('click', async () => {
    const n = shownCount();
    toast(await copyText(shownText()) ? `Copied ${n} ${n === 1 ? 'line' : 'lines'}.` : "Couldn't copy the log.", 'info', 2200);
  });
  $('btnLogSave')?.addEventListener('click', async () => {
    const d = new Date();
    const which = state.session === 'previous' ? 'last-session' : logDate(d) + '-' + logClock(d).slice(0, 5).replace(':', '');
    try {
      const path = await window.radium?.logSave(shownText(), `radium-launcher-log-${which}.txt`);
      if (path) toast('Log saved.', 'ok', 2500);
    } catch (e) {
      toast(String(e), 'error', 4000);
    }
  });
  $('btnLogFolder')?.addEventListener('click', async () => {
    const ok = await window.radium?.logOpenFolder().catch(() => false);
    if (!ok) toast("Couldn't open the log folder.", 'error', 3000);
  });
  $('btnLogClear')?.addEventListener('click', () => {
    state.clearedSeq = logSeq;
    clearedAt = logClock();
    render();
    toast('Cleared. A bug report still includes everything logged this session.', 'info', 3500);
  });

  $('btnReportProblem')?.addEventListener('click', () => openReport());

  return { render };
})();

// ── Report a Problem ─────────────────────────────────────────────────────

/// Words for each kind of problem: what the description box asks for.
const REPORT_PROMPTS = {
  launch:   'What did you press, and what happened instead? Any message on screen?',
  download: 'Where did it stop — downloading, extracting, verifying? What did it say?',
  account:  'Which revival were you signing in to, and what happened?',
  looks:    'Which page and which skin? What looks wrong?',
  launcher: 'What were you doing, and what went wrong?',
  other:    'Tell us what happened, and what you expected instead.',
};

/// A log line's source suggests the kind of problem, when a report is started
/// from one ("Report this").
const REPORT_CAT_FOR_SOURCE = { install: 'download', game: 'launch', account: 'account', update: 'launcher' };

const SEVERITY_NAMES = {
  low: 'Minor', medium: "Something's broken", high: "Can't play", critical: 'Crashes or freezes',
};

const reportDraft = {
  step: 'describe',      // describe | review | done
  category: null,
  severity: 'medium',
  linked: null,          // the log entry a report was started from
  attachLog: true,
  attachPrevious: false,
  sending: false,
};
let reportCooldownTimer = null;
let reportCooldownLeft = 0;
let reportPreviewSeq = 0;

function reportDiagnostics() {
  const skin = $('cfgTheme');
  return {
    // The skin's name as Settings shows it; the backend has only its id.
    themeName: skin?.options[skin.selectedIndex]?.textContent.trim() || '',
    isInstalled,
    isGameRunning,
    isDownloading,
    // A paused or cancelling download is not "not downloading".
    downloadState: isCancelling ? 'cancelling' : isPaused ? 'paused' : isDownloading ? 'downloading' : 'idle',
    errorCount: logEntries.filter(e => e.level === 'error').length,
    apiOnline: lastServerStatus.apiOnline,
    cdnOnline: lastServerStatus.cdnOnline,
  };
}

/// This session's whole log as text: everything the page holds, Clear or no
/// Clear, filters or no filters.
function reportLogText() {
  return logEntries.map(logLineText).join('\n');
}

function reportText() { return $('reportText').value.trim(); }

function updateReportDescribe() {
  const len = reportText().length;
  const count = $('reportTextCount');
  count.textContent = `${$('reportText').value.length} / 1500`;
  const hint = $('reportTextHint');
  let missing = '';
  if (!reportDraft.category) missing = 'Pick what kind of problem it is.';
  else if (len < 10) missing = len ? `A few more words — at least 10 characters.` : '';
  hint.textContent = missing;
  if (reportDraft.step === 'describe') {
    $('reportNext').disabled = !reportDraft.category || len < 10;
  }
}

function setReportCategory(cat) {
  reportDraft.category = cat;
  document.querySelectorAll('.report-cat').forEach(b => {
    const on = b.dataset.cat === cat;
    b.classList.toggle('active', on);
    b.setAttribute('aria-checked', on ? 'true' : 'false');
  });
  $('reportText').placeholder = REPORT_PROMPTS[cat] || REPORT_PROMPTS.other;
  updateReportDescribe();
}

function setReportSeverity(sev) {
  reportDraft.severity = sev;
  document.querySelectorAll('.report-sev-btn').forEach(b => {
    const on = b.dataset.sev === sev;
    b.classList.toggle('active', on);
    b.setAttribute('aria-checked', on ? 'true' : 'false');
  });
}

function renderReportLinked() {
  const box = $('reportLinked');
  const e = reportDraft.linked;
  box.hidden = !e;
  if (!e) return;
  const line = $('reportLinkedLine');
  line.className = `report-linked-line lvl-${e.level}`;
  line.replaceChildren();
  const lvl = document.createElement('span');
  lvl.className = 'log-lvl';
  lvl.textContent = LOG_LEVELS[e.level];
  const msg = document.createElement('span');
  msg.className = 'report-linked-msg';
  msg.textContent = e.msg;
  line.append(lvl, msg);
}

function setToggleSwitch(id, on) {
  const el = $(id);
  el.classList.toggle('on', on);
  el.setAttribute('aria-checked', on ? 'true' : 'false');
}

function showReportStep(step) {
  reportDraft.step = step;
  $('reportPageDescribe').hidden = step !== 'describe';
  $('reportPageReview').hidden = step !== 'review';
  $('reportPageDone').hidden = step !== 'done';
  $('reportModal').querySelector('.report-box').dataset.step = step;

  document.querySelectorAll('.report-step').forEach(s => {
    const n = s.dataset.step;
    s.classList.toggle('active', n === step || (n === 'review' && step === 'done'));
    s.classList.toggle('done', (n === 'describe' && step !== 'describe') || (n === 'review' && step === 'done'));
  });
  $('reportSteps').hidden = step === 'done';

  const back = $('reportBack'), next = $('reportNext');
  back.hidden = step === 'done';
  back.textContent = step === 'review' ? 'Back' : 'Cancel';
  if (step === 'describe') {
    next.textContent = 'Next';
    updateReportDescribe();
  } else if (step === 'review') {
    updateReportSend();
  } else {
    next.disabled = false;
    next.textContent = 'Done';
  }
}

function updateReportSend() {
  const next = $('reportNext');
  if (reportDraft.step !== 'review') return;
  if (reportDraft.sending) {
    next.disabled = true;
    next.textContent = 'Sending…';
  } else if (reportCooldownLeft > 0) {
    next.disabled = true;
    next.textContent = `Wait ${reportCooldownLeft}s`;
  } else {
    next.disabled = false;
    next.textContent = $('reportError').hidden ? 'Send report' : 'Try again';
  }
  $('reportModal').classList.toggle('is-sending', reportDraft.sending);
}

function startReportCooldown(seconds) {
  reportCooldownLeft = Math.max(0, Math.round(seconds));
  clearInterval(reportCooldownTimer);
  updateReportSend();
  if (!reportCooldownLeft) return;
  reportCooldownTimer = setInterval(() => {
    reportCooldownLeft = Math.max(0, reportCooldownLeft - 1);
    if (!reportCooldownLeft) clearInterval(reportCooldownTimer);
    updateReportSend();
  }, 1000);
}

/// The review step: the person's own words, then everything the launcher
/// adds, as the backend will send it.
async function loadReportReview() {
  $('reportSumCat').textContent = document.querySelector(`.report-cat[data-cat="${reportDraft.category}"] .report-cat-name`)?.textContent || '';
  const sev = $('reportSumSev');
  sev.textContent = SEVERITY_NAMES[reportDraft.severity];
  sev.className = `report-sev-pill sev-${reportDraft.severity}`;
  $('reportSumText').textContent = reportText();
  $('reportError').hidden = true;

  const errors = logEntries.filter(e => e.level === 'error').length;
  $('reportLogMeta').textContent = `${logEntries.length} ${logEntries.length === 1 ? 'line' : 'lines'}`
    + (errors ? ` · ${errors} ${errors === 1 ? 'error' : 'errors'}` : '');
  setToggleSwitch('tglReportLog', reportDraft.attachLog);

  const facts = $('reportFacts');
  facts.classList.add('is-loading');
  const seq = ++reportPreviewSeq;
  let preview = null;
  try {
    preview = await window.radium?.bugReportPreview(reportDiagnostics(), reportLogText());
  } catch (e) {}
  if (seq !== reportPreviewSeq) return;
  facts.classList.remove('is-loading');
  facts.replaceChildren();
  for (const f of preview?.facts || []) {
    const dt = document.createElement('dt');
    dt.textContent = f.label;
    const dd = document.createElement('dd');
    dd.textContent = f.value;
    if (/OUTDATED|OFFLINE|Not installed/.test(f.value)) dd.classList.add('is-bad');
    facts.append(dt, dd);
  }
  $('reportPeekCurrent').textContent = preview?.logPreview || '(nothing logged yet)';

  const prev = preview?.previous;
  $('reportPrevRow').hidden = !prev;
  $('reportPeekPrevBtn').hidden = !prev;
  if (prev) {
    $('reportPrevMeta').textContent = `${prev.endingText} · ${prev.lines} ${prev.lines === 1 ? 'line' : 'lines'}`;
    $('reportPrevMeta').classList.toggle('is-bad', prev.ending !== 'closed');
    $('reportPeekPrevious').textContent = prev.preview;
    // Offered by default when that session ended badly: it is probably the
    // one being reported.
    if (reportDraft.attachPrevious === null) reportDraft.attachPrevious = prev.ending !== 'closed';
  } else {
    reportDraft.attachPrevious = false;
  }
  setToggleSwitch('tglReportPrev', !!reportDraft.attachPrevious);
  startReportCooldown(preview?.cooldown || 0);
}

async function sendReport() {
  if (reportDraft.sending) return;
  reportDraft.sending = true;
  $('reportError').hidden = true;
  updateReportSend();
  try {
    const sent = await window.radium.submitBugReport({
      description: reportText(),
      category: reportDraft.category,
      severity: reportDraft.severity,
      logs: reportDraft.attachLog ? reportLogText() : '',
      attachLog: reportDraft.attachLog,
      attachPrevious: !!reportDraft.attachPrevious,
      linkedLine: reportDraft.linked ? (reportDraft.linked.line || logLineText(reportDraft.linked)) : null,
      diagnostics: reportDiagnostics(),
    });
    reportDraft.sending = false;
    addLog(`Bug report ${sent.reference} sent.`, 'ok');
    startReportCooldown(60);
    showReportStep('done');
    resetReportDraft();
  } catch (err) {
    reportDraft.sending = false;
    const box = $('reportError');
    box.textContent = String(err || "The report couldn't be sent.");
    box.hidden = false;
    updateReportSend();
  }
}

function resetReportDraft() {
  $('reportText').value = '';
  reportDraft.category = null;
  reportDraft.linked = null;
  reportDraft.attachLog = true;
  reportDraft.attachPrevious = null;
  setReportCategory(null);
  setReportSeverity('medium');
  renderReportLinked();
  document.querySelectorAll('.report-peek-box').forEach(b => { b.hidden = true; });
  document.querySelectorAll('.report-peek').forEach(b => { b.textContent = "Show what's in it"; });
}

/// Open the dialog. `linked` is a log entry when started from "Report this".
/// A draft left by Cancel is kept and comes back.
function openReport({ linked = null } = {}) {
  const modal = $('reportModal');
  if (!modal) return;
  if (reportDraft.step === 'done') resetReportDraft();
  if (linked) {
    reportDraft.linked = linked;
    if (!reportDraft.category && REPORT_CAT_FOR_SOURCE[linked.source]) {
      setReportCategory(REPORT_CAT_FOR_SOURCE[linked.source]);
    }
    if (linked.level === 'error' && reportDraft.severity === 'medium' && linked.msg.startsWith('Crashed')) {
      setReportSeverity('critical');
    }
  }
  renderReportLinked();
  showReportStep('describe');
  showModal(modal);
  (window.radium?.bugReportCooldown?.() || Promise.resolve(0))
    .then(left => { if (left > reportCooldownLeft) startReportCooldown(left); })
    .catch(() => {});
  setTimeout(() => {
    (reportDraft.category ? $('reportText') : document.querySelector('.report-cat'))?.focus();
  }, 30);
}

function closeReport() {
  if (reportDraft.sending) return;
  hideModal($('reportModal'), () => {
    if (reportDraft.step === 'done') showReportStep('describe');
  });
}

(function setupReportDialog() {
  const modal = $('reportModal');
  if (!modal) return;
  reportDraft.attachPrevious = null;

  document.querySelectorAll('.report-cat').forEach(b => {
    b.addEventListener('click', () => {
      setReportCategory(b.dataset.cat);
      $('reportText').focus();
    });
  });
  document.querySelectorAll('.report-sev-btn').forEach(b => {
    b.addEventListener('click', () => setReportSeverity(b.dataset.sev));
  });
  // Arrow keys move through a radio group, as they do in a native one.
  for (const group of modal.querySelectorAll('[role="radiogroup"]')) {
    group.addEventListener('keydown', (ev) => {
      const keys = { ArrowRight: 1, ArrowDown: 1, ArrowLeft: -1, ArrowUp: -1 };
      if (!(ev.key in keys)) return;
      const items = [...group.querySelectorAll('[role="radio"]')];
      const i = items.indexOf(document.activeElement);
      if (i < 0) return;
      ev.preventDefault();
      const next = items[(i + keys[ev.key] + items.length) % items.length];
      next.focus();
      next.click();
    });
  }
  $('reportText').addEventListener('input', updateReportDescribe);
  $('reportText').addEventListener('keydown', (ev) => {
    if (ev.key === 'Enter' && (ev.ctrlKey || ev.metaKey) && !$('reportNext').disabled) $('reportNext').click();
  });
  $('reportLinkedRemove').addEventListener('click', () => {
    reportDraft.linked = null;
    renderReportLinked();
  });

  for (const [id, key] of [['tglReportLog', 'attachLog'], ['tglReportPrev', 'attachPrevious']]) {
    const el = $(id);
    const flip = () => {
      reportDraft[key] = !reportDraft[key];
      setToggleSwitch(id, reportDraft[key]);
    };
    el.addEventListener('click', flip);
    el.addEventListener('keydown', (ev) => {
      if (ev.key === ' ' || ev.key === 'Enter') { ev.preventDefault(); flip(); }
    });
  }
  document.querySelectorAll('.report-peek').forEach(btn => {
    btn.addEventListener('click', () => {
      const box = $(btn.dataset.peek);
      box.hidden = !box.hidden;
      btn.textContent = box.hidden ? "Show what's in it" : 'Hide';
      if (!box.hidden) box.scrollTop = box.scrollHeight;
    });
  });

  $('reportNext').addEventListener('click', () => {
    if (reportDraft.step === 'describe') {
      showReportStep('review');
      loadReportReview();
    } else if (reportDraft.step === 'review') {
      sendReport();
    } else {
      closeReport();
    }
  });
  $('reportBack').addEventListener('click', () => {
    if (reportDraft.step === 'review' && !reportDraft.sending) {
      reportPreviewSeq++;
      showReportStep('describe');
      $('reportText').focus();
    } else {
      closeReport();
    }
  });
  $('reportEdit').addEventListener('click', () => $('reportBack').click());
  $('reportClose').addEventListener('click', closeReport);
  modal.addEventListener('keydown', (ev) => {
    if (ev.key === 'Escape') { ev.preventDefault(); closeReport(); }
  });

  setReportSeverity('medium');
  updateReportDescribe();
})();
