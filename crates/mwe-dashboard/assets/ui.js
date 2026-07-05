// ui.js — UI shell toggles for the dashboard. Loaded with `defer` on
// every authenticated page, alongside chat.js. Owns two responsibilities
// kept separate from chat.js (which owns the chat *content*):
//
//   1. Mobile hamburger — the `#nav-toggle` button toggles a `.nav-open`
//      class on `#site-nav`; the components layer of `tailwind/app.css`
//      reveals the stacked nav under the topbar on viewports < md.
//   2. Chat panel show/hide — `#chat-close` (inside the panel header)
//      and `#chat-reopen` (the floating reopen FAB) drive a single
//      `body.chat-open` class. The state is persisted under
//      `localStorage.mwe-mcp.chat.open` ('1' = explicit open, '0' =
//      explicit closed); when no value is stored we default to "open"
//      on viewports ≥ 1280 px (xl breakpoint matches tailwind/app.css)
//      and "closed" on smaller viewports. Width persistence and drag
//      resize belong to chat.js.

(function () {
  'use strict';

  // ---- responsive table wrapping ------------------------------------------
  //
  // Wrap every `<table>` inside <main> with a `<div class="table-wrap">`
  // so the wrapper scrolls horizontally on narrow viewports instead of
  // the page overflowing. The table itself stays as native
  // `display: table`, which keeps thead and tbody on the same column
  // layout — headers stay aligned with row cells regardless of how
  // wide the columns get. Idempotent: skips tables whose parent
  // already carries the class (re-running this code is harmless if
  // a future hot-reload re-fires it).

  for (const t of document.querySelectorAll('main table')) {
    if (t.parentElement && t.parentElement.classList.contains('table-wrap')) continue;
    const wrap = document.createElement('div');
    wrap.className = 'table-wrap';
    t.parentNode.insertBefore(wrap, t);
    wrap.appendChild(t);
  }

  // ---- mobile hamburger ----------------------------------------------------

  const navToggle = document.getElementById('nav-toggle');
  const siteNav = document.getElementById('site-nav');
  if (navToggle && siteNav) {
    navToggle.addEventListener('click', function () {
      const open = siteNav.classList.toggle('nav-open');
      navToggle.setAttribute('aria-expanded', open ? 'true' : 'false');
    });
  }

  // ---- chat panel show/hide ------------------------------------------------

  const CHAT_OPEN_KEY = 'mwe-mcp.chat.open';
  const CHAT_XL_BREAKPOINT = 1280;

  const chatClose = document.getElementById('chat-close');
  const chatReopen = document.getElementById('chat-reopen');
  const body = document.body;

  if (body.classList.contains('has-chat-panel')) {
    const stored = (function () {
      try {
        return localStorage.getItem(CHAT_OPEN_KEY);
      } catch (_e) {
        return null;
      }
    })();
    let isOpen;
    if (stored === '1') isOpen = true;
    else if (stored === '0') isOpen = false;
    else isOpen = window.innerWidth >= CHAT_XL_BREAKPOINT;
    body.classList.toggle('chat-open', isOpen);
  }

  function setChatOpen(open) {
    body.classList.toggle('chat-open', open);
    try {
      localStorage.setItem(CHAT_OPEN_KEY, open ? '1' : '0');
    } catch (_e) {
      // Quota exceeded or storage disabled — keep going in-memory.
    }
  }

  // Expose the canonical "open the panel" action so the other deferred
  // script (chat.js, wiring the in-flight badge) can open the chat through
  // the same state machine + persistence instead of poking the body class.
  window.mweChatOpen = function () { setChatOpen(true); };

  if (chatClose) {
    chatClose.addEventListener('click', function () {
      setChatOpen(false);
    });
  }
  if (chatReopen) {
    chatReopen.addEventListener('click', function () {
      setChatOpen(true);
    });
  }

  // ---- dream forms: run in the background, animate a topnav indicator -----
  //
  // The three dream triggers live on the /dashboard/dream console page. A dream
  // can run for many seconds, so instead of a synchronous full-page POST that
  // freezes the page, each form POSTs via fetch with `Accept: application/json`:
  // the server kicks the dream off on a background task (guarded by the REM
  // gate) and acks immediately. We show an animated "dream…" pill in the topnav
  // (#dream-indicator), poll /dashboard/dream/status until it goes idle, then —
  // when we are on the console page — reload so the new run shows up in the
  // history table; elsewhere we show the one-line outcome (click to dismiss).
  // No-JS users get the synchronous HTML report for all three (the server
  // branches on the Accept header), so the surface still degrades cleanly.

  const dreamIndicator = document.getElementById('dream-indicator');
  let dreamDots = null;
  let dreamPoll = null;

  function startDreamDots() {
    if (!dreamIndicator) return;
    dreamIndicator.onclick = null;
    dreamIndicator.style.cursor = 'default';
    dreamIndicator.style.color = 'var(--bg)';
    dreamIndicator.style.background = 'var(--p)';
    dreamIndicator.style.display = 'inline-flex';
    dreamIndicator.title = 'A dream is running';
    let n = 0;
    const tick = function () {
      dreamIndicator.textContent = 'dream' + '.'.repeat(n % 4);
      n += 1;
    };
    tick();
    if (dreamDots) clearInterval(dreamDots);
    dreamDots = setInterval(tick, 450);
  }

  function finishDream(last) {
    if (dreamDots) { clearInterval(dreamDots); dreamDots = null; }
    if (!dreamIndicator) return;
    if (!last) { dreamIndicator.style.display = 'none'; return; }
    const full =
      (last.ok ? 'dream ✓ ' : 'dream ✗ ') +
      (last.kind || '') +
      (last.summary ? ' · ' + last.summary : '');
    dreamIndicator.style.color = 'var(--bg)';
    dreamIndicator.style.background = last.ok ? 'var(--p)' : 'var(--rose)';
    dreamIndicator.textContent = full.length > 90 ? full.slice(0, 89) + '…' : full;
    dreamIndicator.style.cursor = 'pointer';
    dreamIndicator.title = full + ' — click to dismiss';
    dreamIndicator.onclick = function () {
      dreamIndicator.style.display = 'none';
      dreamIndicator.onclick = null;
    };
  }

  function pollDreamStatus() {
    if (dreamPoll) return;
    dreamPoll = setInterval(function () {
      fetch('/dashboard/dream/status', {
        headers: { Accept: 'application/json' },
        credentials: 'same-origin',
      })
        .then(function (r) { return r.ok ? r.json() : null; })
        .then(function (data) {
          if (!data) return;
          if (data.running) {
            if (!dreamDots) startDreamDots();
          } else {
            clearInterval(dreamPoll);
            dreamPoll = null;
            // On the console page the history table is the record — reload so
            // the just-finished run appears with its log. Elsewhere just show
            // the one-line outcome pill.
            if (document.getElementById('dream-history')) {
              location.reload();
              return;
            }
            finishDream(data.last || null);
          }
        })
        .catch(function () { /* transient — keep polling */ });
    }, 1500);
  }

  for (const form of document.querySelectorAll('form.dream-form')) {
    const action = form.getAttribute('action') || '';
    form.addEventListener('submit', function (ev) {
      if (!window.fetch) return; // ancient browser — let it POST normally
      ev.preventDefault();
      startDreamDots();
      fetch(action, {
        method: 'POST',
        headers: { Accept: 'application/json' },
        credentials: 'same-origin',
      })
        .then(function (r) { return r.ok ? r.json() : null; })
        .then(function () {
          // Whether we started it or it was already busy, a dream is running —
          // poll to the shared end.
          pollDreamStatus();
        })
        .catch(function () { finishDream(null); });
    });
  }

  // On load, resume the indicator if a dream is already running (started in
  // another tab, or still going after a navigation).
  if (dreamIndicator && window.fetch) {
    fetch('/dashboard/dream/status', {
      headers: { Accept: 'application/json' },
      credentials: 'same-origin',
    })
      .then(function (r) { return r.ok ? r.json() : null; })
      .then(function (data) {
        if (data && data.running) { startDreamDots(); pollDreamStatus(); }
      })
      .catch(function () {});
  }

  // ---- dream run log modal (console history) ------------------------------
  //
  // Each row of the Dream history table carries a "log" button
  // (`button.dream-log-open[data-run-id]`). Clicking it fetches that run's
  // fragment from /dashboard/dream/runs/<id> into the modal body
  // (#dream-log-body) and reveals the modal (#dream-log-modal). Close on the ×
  // button, a backdrop click, or Escape. Delegated on document so it covers
  // every row with one listener; a no-op on pages without the modal.

  const dreamLogModal = document.getElementById('dream-log-modal');
  if (dreamLogModal && window.fetch) {
    const dreamLogBody = document.getElementById('dream-log-body');
    const closeDreamLog = function () { dreamLogModal.style.display = 'none'; };
    document.addEventListener('click', function (ev) {
      const btn = ev.target.closest ? ev.target.closest('.dream-log-open') : null;
      if (!btn) return;
      const id = btn.getAttribute('data-run-id');
      if (!id) return;
      if (dreamLogBody) dreamLogBody.innerHTML = '<p class="muted">Loading…</p>';
      dreamLogModal.style.display = 'flex';
      fetch('/dashboard/dream/runs/' + encodeURIComponent(id), {
        headers: { Accept: 'text/html' },
        credentials: 'same-origin',
        cache: 'no-store',
      })
        .then(function (r) {
          return r.ok ? r.text() : Promise.reject(new Error('HTTP ' + r.status));
        })
        .then(function (htmlText) {
          if (dreamLogBody) dreamLogBody.innerHTML = htmlText;
        })
        .catch(function () {
          if (dreamLogBody) {
            dreamLogBody.innerHTML =
              '<p class="flash flash-error">Could not load this run\'s log.</p>';
          }
        });
    });
    dreamLogModal.addEventListener('click', function (ev) {
      if (ev.target === dreamLogModal) closeDreamLog();
    });
    const dreamLogClose = document.getElementById('dream-log-close');
    if (dreamLogClose) dreamLogClose.addEventListener('click', closeDreamLog);
    document.addEventListener('keydown', function (ev) {
      if (ev.key === 'Escape' && dreamLogModal.style.display === 'flex') closeDreamLog();
    });
  }

  // ---- operative-chat help modal ------------------------------------------
  //
  // The "Help" button (#help-open) lives in the chat panel header, between
  // the title and the close (×). It is a real anchor to /dashboard/help (the
  // no-JS fallback page). With JS we intercept the click and reveal the
  // modal (#help-modal, rendered in the shell for every authenticated user).
  // Same close affordances as the dream modal: × button, backdrop, Escape.

  const helpOpen = document.getElementById('help-open');
  const helpModal = document.getElementById('help-modal');
  if (helpOpen && helpModal) {
    const closeHelp = function () { helpModal.style.display = 'none'; };
    helpOpen.addEventListener('click', function (ev) {
      ev.preventDefault();
      helpModal.style.display = 'flex';
    });
    helpModal.addEventListener('click', function (ev) {
      if (ev.target === helpModal) closeHelp();
    });
    const helpClose = document.getElementById('help-close');
    if (helpClose) helpClose.addEventListener('click', closeHelp);
    document.addEventListener('keydown', function (ev) {
      if (ev.key === 'Escape' && helpModal.style.display === 'flex') closeHelp();
    });
  }

  // ---- in-flight badge -----------------------------------------------------
  //
  // The shell layout is a pure sync render and cannot touch the DB, so the
  // topnav in-flight badge (#in-flight-badge) starts hidden (style.display:
  // none) and we fetch its count client-side.
  // /dashboard/proposals/in-flight-count is ACL-scoped to the signed-in user
  // and returns { pending, applied_pending_confirm, revertable_applied,
  // total }. When total > 0 we reveal the badge with the count
  // ("N pending"); otherwise it stays hidden. Clicking it follows the
  // anchor's href, which lands the operator in the chat on those items.

  const inFlightBadge = document.getElementById('in-flight-badge');
  const inFlightCount = document.getElementById('in-flight-badge-count');
  if (inFlightBadge && inFlightCount && window.fetch) {
    fetch('/dashboard/proposals/in-flight-count', {
      headers: { Accept: 'application/json' },
      credentials: 'same-origin',
    })
      .then(function (resp) { return resp.ok ? resp.json() : null; })
      .then(function (data) {
        if (!data || typeof data.total !== 'number' || data.total <= 0) return;
        inFlightCount.textContent = data.total + ' pending';
        inFlightBadge.style.display = 'inline-flex';
      })
      .catch(function () {
        // Network/serialisation failure — leave the badge hidden; the
        // chat ("what's pending?") still surfaces the same state.
      });
  }

  // ---- health page: async LLM-slot probe ----------------------------------
  //
  // The Health page paints instantly with the fast DB/workdir diagnostics and
  // leaves a spinner in #llm-slots: the per-slot LLM reachability probe makes a
  // network round-trip per slot and can hang on an unreachable backend, so
  // running it inline would stall the whole page. Fetch the probed table from
  // /dashboard/admin/health/llm-slots?fragment=1 and swap it in when it
  // arrives. No-JS users get the <noscript> link to the full-page version.

  const llmSlots = document.getElementById('llm-slots');
  if (llmSlots && window.fetch) {
    fetch('/dashboard/admin/health/llm-slots?fragment=1', {
      headers: { Accept: 'text/html' },
      credentials: 'same-origin',
      cache: 'no-store',
    })
      .then(function (r) {
        return r.ok ? r.text() : Promise.reject(new Error('HTTP ' + r.status));
      })
      .then(function (htmlText) {
        llmSlots.innerHTML = htmlText;
        // Wrap the injected table for horizontal scroll like the page's other
        // tables (the load-time wrap pass ran before this table existed).
        const t = llmSlots.querySelector('table');
        if (t && !(t.parentElement && t.parentElement.classList.contains('table-wrap'))) {
          const wrap = document.createElement('div');
          wrap.className = 'table-wrap';
          t.parentNode.insertBefore(wrap, t);
          wrap.appendChild(t);
        }
      })
      .catch(function () {
        llmSlots.innerHTML =
          '<p class="flash flash-error">Could not load LLM slot diagnostics. ' +
          '<a href="/dashboard/admin/health/llm-slots">Open them on their own page</a>.</p>';
      });
  }

  // ---- click-to-copy fact ids (facts browser) ------------------------------
  //
  // The facts table shows each fact id abbreviated (`019ef28e…`) inside a
  // `<code class="copy-id" data-fact-id="<full>">`. Clicking copies the full
  // id to the clipboard and briefly flips the cell to a confirmation. Pure
  // delegation on document, so it covers every row (and any re-render) with
  // one listener; a no-op where the Clipboard API is unavailable.

  document.addEventListener('click', function (ev) {
    const el = ev.target.closest ? ev.target.closest('code.copy-id') : null;
    if (!el) return;
    const id = el.getAttribute('data-fact-id');
    if (!id) return;
    const restore = el.textContent;
    const confirm = function () {
      el.classList.add('copied');
      el.textContent = 'copiato!';
      setTimeout(function () {
        el.textContent = restore;
        el.classList.remove('copied');
      }, 900);
    };
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(id).then(confirm).catch(function () {});
      return;
    }
    // Legacy fallback: a hidden textarea + execCommand('copy').
    try {
      const ta = document.createElement('textarea');
      ta.value = id;
      ta.setAttribute('readonly', '');
      ta.style.position = 'absolute';
      ta.style.left = '-9999px';
      document.body.appendChild(ta);
      ta.select();
      document.execCommand('copy');
      document.body.removeChild(ta);
      confirm();
    } catch (_e) {
      // Clipboard unavailable — leave the id visible for manual copy.
    }
  });
})();
