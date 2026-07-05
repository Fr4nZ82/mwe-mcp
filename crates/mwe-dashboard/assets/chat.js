// chat.js — client side of the persistent chat panel (I.9 + I.10).
//
// Responsibilities:
//   - Hydrate the message list from localStorage on page load (FIFO,
//     max MAX_ENTRIES turns).
//   - Intercept the form submit, POST to /dashboard/chat/agentic with
//     Accept: application/json, render the returned AgenticTurn
//     (tool-call trace bubbles + final assistant message), and
//     persist.
//   - Drag-resize the panel from its left edge; width is persisted.
//   - Splice in a one-shot primer turn published by /dashboard/welcome
//     via window.__mweChatPrimer (consumed exactly once).
//
// Two flavours of stored entry coexist in localStorage:
//
//   - Agentic turn (the standard chat submit, I.10):
//       { user_text, trace, final_message, final_message_html,
//         iterations, budget_exhausted, ts }
//     `final_message` is the raw reply (replayed into the next turn's
//     confirmation window); `final_message_html` is the same text
//     rendered server-side to safe HTML, and is what the bubble shows so
//     the model's markdown reads like a normal chat. Entries stored
//     before HTML rendering existed carry no `final_message_html` and
//     fall back to the raw-text bubble.
//   - Welcome primer (I.9, server-rendered ingest fragment):
//       { user_text, response_html, ts }
//
// The render function detects which shape it has and routes to the
// matching DOM builder. Fact continuity across turns still relies on
// wiki_recall + autocapture, NOT on replaying scrollback. But the
// agentic chat does need the immediately-preceding exchange so a
// confirmation ("sì") resolves against the assistant's prior proposal:
// each submit replays a bounded `{user, assistant}` window (see
// `recentTurns`) to the server, which clamps it further. The rest of
// the localStorage history is purely the user's scrollback.

(function () {
  'use strict';

  // Per-user history namespace: the panel's scrollback must not leak
  // across accounts on a shared browser, nor survive into a different
  // user. `window.__mweUser` is set by the page shell for the signed-in
  // user; fall back to `anon` only if it is somehow absent.
  const userKey = (window.__mweUser && String(window.__mweUser)) || 'anon';
  const STORAGE_KEY = 'mwe-mcp.chat.history.' + userKey;
  const WIDTH_KEY = 'mwe-mcp.chat.width';
  const MAX_ENTRIES = 100;
  const MIN_WIDTH = 280;
  const MAX_WIDTH = 720;

  const panel = document.getElementById('chat-panel');
  if (!panel) return;
  const messages = document.getElementById('chat-panel-messages');
  const form = document.getElementById('chat-panel-form');
  const textarea = document.getElementById('chat-panel-text');
  const handle = document.getElementById('chat-panel-resize');
  if (!messages || !form || !textarea || !handle) return;

  // ---- width hydration -----------------------------------------------------
  // The body's reserved right-padding is governed by the CSS rule
  // `body.has-chat-panel.chat-open { padding-right: var(--chat-panel-width) }`
  // — only active when ui.js has applied the `chat-open` class AND
  // the viewport is at the xl breakpoint. The rule reads the
  // `--chat-panel-width` CSS variable; we just update the variable
  // here so user-resized widths persist. **Never** touch
  // `document.body.style.paddingRight` directly: an inline style
  // overrides the CSS rule and the body would keep reserving space
  // even after the chat is closed (regression observed 2026-05-26
  // on viewports < xl, where the user closed the chat but the
  // body stayed padded from the saved width hydration).
  const savedWidth = localStorage.getItem(WIDTH_KEY);
  if (savedWidth) {
    const w = parseInt(savedWidth, 10);
    if (Number.isFinite(w) && w >= MIN_WIDTH && w <= MAX_WIDTH) {
      panel.style.width = w + 'px';
      document.body.style.setProperty('--chat-panel-width', w + 'px');
    }
  }

  // ---- history hydration ---------------------------------------------------
  function loadHistory() {
    try {
      const raw = localStorage.getItem(STORAGE_KEY);
      if (!raw) return [];
      const parsed = JSON.parse(raw);
      return Array.isArray(parsed) ? parsed : [];
    } catch (_e) {
      return [];
    }
  }

  function saveHistory(arr) {
    while (arr.length > MAX_ENTRIES) arr.shift();
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(arr));
    } catch (_e) {
      // Quota exceeded or storage disabled — keep going in-memory.
    }
  }

  function truncate(str, n) {
    if (typeof str !== 'string') return '';
    if (str.length <= n) return str;
    return str.slice(0, n) + '…';
  }

  function renderUserBubble(text) {
    const u = document.createElement('div');
    u.className = 'chat-panel-user';
    u.textContent = text || '';
    return u;
  }

  // Pretty-print a JSON value for the (scrollable) detail blocks.
  function prettyJson(value) {
    try {
      return JSON.stringify(value, null, 2);
    } catch (_e) {
      return String(value);
    }
  }

  // A tool result arrives as a JSON string; indent it for readability,
  // falling back to the raw text when it does not parse.
  function prettyResult(str) {
    if (typeof str !== 'string') return '';
    try {
      return JSON.stringify(JSON.parse(str), null, 2);
    } catch (_e) {
      return str;
    }
  }

  // One-line hint for the collapsed tool header — the arguments as
  // compact `key=value` pairs (e.g. `status=pending`), trimmed so the
  // header stays a single readable line instead of a JSON dump.
  function toolArgsHint(args) {
    if (!args || typeof args !== 'object') return '';
    const parts = [];
    for (const [k, v] of Object.entries(args)) {
      parts.push(k + '=' + (v && typeof v === 'object' ? JSON.stringify(v) : String(v)));
    }
    return truncate(parts.join(' · '), 72);
  }

  // Tool-call trace entry. A collapsed `<details>` keeps the transcript a
  // clean, scannable list of which `_internal.*` tools ran; expanding one
  // reveals the full, pretty-printed arguments + result. The
  // `.chat-panel-tool-result` style already wraps + scrolls (max-height
  // 12rem), so nothing is truncated — this replaces the old single-line,
  // cut-off JSON dump.
  function renderToolBubble(call) {
    const tool = document.createElement('details');
    tool.className = 'chat-panel-tool' + (call.is_error ? ' is-error' : '');

    const summary = document.createElement('summary');
    summary.className = 'chat-panel-tool-header';
    summary.style.cursor = 'pointer';
    summary.style.listStyle = 'none';

    const caret = document.createElement('span');
    caret.className = 'chat-panel-tool-args';
    caret.textContent = '▸';
    summary.appendChild(caret);

    const name = document.createElement('code');
    name.className = 'chat-panel-tool-name';
    name.textContent = call.name || '(tool)';
    summary.appendChild(name);

    const hint = document.createElement('span');
    hint.className = 'chat-panel-tool-args';
    hint.textContent = call.is_error ? 'error' : toolArgsHint(call.arguments);
    summary.appendChild(hint);
    tool.appendChild(summary);

    tool.addEventListener('toggle', function () {
      caret.textContent = tool.open ? '▾' : '▸';
    });

    if (
      call.arguments &&
      typeof call.arguments === 'object' &&
      Object.keys(call.arguments).length > 0
    ) {
      const args = document.createElement('pre');
      args.className = 'chat-panel-tool-result';
      args.textContent = 'arguments\n' + prettyJson(call.arguments);
      tool.appendChild(args);
    }

    if (call.result) {
      const result = document.createElement('pre');
      result.className = 'chat-panel-tool-result';
      result.textContent = 'result\n' + prettyResult(call.result);
      tool.appendChild(result);
    }
    return tool;
  }

  function renderBotBubble(text) {
    const r = document.createElement('div');
    r.className = 'chat-panel-bot';
    r.textContent = text || '';
    return r;
  }

  function renderHtmlBotBubble(html) {
    const r = document.createElement('div');
    r.className = 'chat-panel-bot';
    r.innerHTML = html || '';
    return r;
  }

  function renderBudgetExhaustedBubble() {
    const w = document.createElement('div');
    w.className = 'chat-panel-error';
    w.textContent =
      'Iteration budget exhausted — the model did not produce a final reply. ' +
      'Try a simpler or more specific request.';
    return w;
  }

  function renderEntry(entry) {
    const turn = document.createElement('div');
    turn.className = 'chat-panel-turn';
    turn.appendChild(renderUserBubble(entry.user_text));

    // Welcome-wizard primer turns carry a pre-rendered HTML fragment.
    if (typeof entry.response_html === 'string') {
      turn.appendChild(renderHtmlBotBubble(entry.response_html));
      messages.appendChild(turn);
      return;
    }

    // Agentic turn: tool-call trace bubbles, then the final assistant
    // message (or a budget-exhausted notice).
    if (Array.isArray(entry.trace)) {
      for (const call of entry.trace) {
        turn.appendChild(renderToolBubble(call));
      }
    }
    if (entry.final_message) {
      // Prefer the server-rendered markdown HTML; fall back to the raw
      // text for entries persisted before HTML rendering existed.
      if (typeof entry.final_message_html === 'string' && entry.final_message_html.trim() !== '') {
        turn.appendChild(renderHtmlBotBubble(entry.final_message_html));
      } else {
        turn.appendChild(renderBotBubble(entry.final_message));
      }
    } else if (entry.budget_exhausted) {
      turn.appendChild(renderBudgetExhaustedBubble());
    }
    messages.appendChild(turn);
  }

  function scrollToBottom() {
    messages.scrollTop = messages.scrollHeight;
  }

  // The recent `{user, assistant}` window replayed to the agentic loop so
  // a confirmation resolves against the assistant's prior proposal. Only
  // agentic turns with a real final reply qualify — primer turns
  // (response_html) and error / budget-exhausted turns (empty
  // final_message) carry no assistant text to replay. The server clamps
  // this further (count + per-message length); keep the wire payload
  // small by sending only the tail here.
  const HISTORY_WINDOW = 6;
  function recentTurns(hist) {
    const out = [];
    for (const e of hist) {
      if (e && typeof e.user_text === 'string' && typeof e.final_message === 'string'
          && e.final_message.trim() !== '') {
        out.push({ user: e.user_text, assistant: e.final_message });
      }
    }
    return out.slice(-HISTORY_WINDOW);
  }

  const history = loadHistory();
  for (const entry of history) renderEntry(entry);

  // Consume a primer turn left behind by /dashboard/welcome, exactly once.
  if (window.__mweChatPrimer && typeof window.__mweChatPrimer === 'object') {
    const primer = window.__mweChatPrimer;
    delete window.__mweChatPrimer;
    history.push(primer);
    saveHistory(history);
    renderEntry(primer);
  }
  scrollToBottom();

  // ---- submit interception -------------------------------------------------
  form.addEventListener('submit', async function (ev) {
    ev.preventDefault();
    const text = textarea.value.trim();
    if (!text) return;

    const submitBtn = form.querySelector('button[type="submit"]');
    const originalLabel = submitBtn.textContent;
    // Visual contract is shared with the welcome wizard: a `.spinner`
    // inside the disabled submit button, textarea disabled too so a
    // second submit cannot race the in-flight POST.
    submitBtn.disabled = true;
    textarea.disabled = true;
    submitBtn.innerHTML = '<span class="spinner"></span>';

    try {
      // URLSearchParams (not FormData): FormData makes fetch send
      // multipart/form-data, which the server's axum::Form<ChatSubmission>
      // extractor rejects with 415. URLSearchParams sends
      // application/x-www-form-urlencoded — the content type Form wants,
      // and the same wire shape as the no-JS `post_message` fallback.
      const body = new URLSearchParams();
      body.set('text', text);
      // Replay the recent exchange so the confirm → act handshake works.
      // `history` here is the scrollback BEFORE this submit (the new turn
      // is pushed only after the response lands), so it carries prior
      // turns only.
      body.set('history', JSON.stringify(recentTurns(history)));
      const res = await fetch('/dashboard/chat/agentic', {
        method: 'POST',
        headers: { Accept: 'application/json' },
        body: body,
        credentials: 'same-origin',
      });
      if (!res.ok) {
        let detail = 'HTTP ' + res.status;
        try {
          const j = await res.json();
          if (j && j.error) detail = j.error;
        } catch (_e) { /* keep status-only */ }
        throw new Error(detail);
      }
      const data = await res.json();
      const entry = {
        user_text: data.user_text || text,
        trace: Array.isArray(data.trace) ? data.trace : [],
        final_message: typeof data.final_message === 'string' ? data.final_message : '',
        final_message_html:
          typeof data.final_message_html === 'string' ? data.final_message_html : '',
        iterations: typeof data.iterations === 'number' ? data.iterations : 0,
        budget_exhausted: !!data.budget_exhausted,
        ts: Date.now(),
      };
      history.push(entry);
      saveHistory(history);
      renderEntry(entry);
      scrollToBottom();
      textarea.value = '';
    } catch (e) {
      const err = document.createElement('div');
      err.className = 'chat-panel-error';
      err.textContent = 'Error: ' + (e && e.message ? e.message : 'request failed');
      messages.appendChild(err);
      scrollToBottom();
    } finally {
      submitBtn.disabled = false;
      textarea.disabled = false;
      submitBtn.textContent = originalLabel;
    }
  });

  // ---- in-flight badge → load the overview turn inline ---------------------
  //
  // The topnav badge (revealed by ui.js with the pending count) opens the
  // chat panel and loads the "what do I have in flight?" overview as a normal
  // turn, with a spinner while the (LLM-backed) summary is composed. It used
  // to navigate to a full landing page that re-rendered the whole dashboard
  // around the chat and linked on to /dashboard/chat — a confusing round-trip.
  // The turn now comes from /dashboard/proposals/in-flight/chat-turn as JSON
  // and renders exactly like any other agentic turn.
  const IN_FLIGHT_LABEL = 'What do I have in flight?';
  const inFlightBadge = document.getElementById('in-flight-badge');
  if (inFlightBadge && window.fetch) {
    inFlightBadge.addEventListener('click', function (ev) {
      ev.preventDefault();
      if (inFlightBadge.dataset.loading === '1') return; // ignore double-click
      inFlightBadge.dataset.loading = '1';
      if (typeof window.mweChatOpen === 'function') window.mweChatOpen();

      // Optimistic pending turn: the question bubble plus a spinner where the
      // reply will land, replaced when the overview arrives.
      const pending = document.createElement('div');
      pending.className = 'chat-panel-turn';
      pending.appendChild(renderUserBubble(IN_FLIGHT_LABEL));
      const wait = document.createElement('div');
      wait.className = 'chat-panel-bot';
      wait.innerHTML = '<span class="spinner"></span>';
      pending.appendChild(wait);
      messages.appendChild(pending);
      scrollToBottom();

      fetch('/dashboard/proposals/in-flight/chat-turn', {
        headers: { Accept: 'application/json' },
        credentials: 'same-origin',
        cache: 'no-store',
      })
        .then(function (res) {
          if (!res.ok) throw new Error('HTTP ' + res.status);
          return res.json();
        })
        .then(function (data) {
          if (pending.parentNode) messages.removeChild(pending);
          // The server's user_text is the verbose primer; show the friendly
          // label instead (it is also what rides the replay window).
          const entry = {
            user_text: IN_FLIGHT_LABEL,
            trace: Array.isArray(data.trace) ? data.trace : [],
            final_message: typeof data.final_message === 'string' ? data.final_message : '',
            final_message_html:
              typeof data.final_message_html === 'string' ? data.final_message_html : '',
            iterations: typeof data.iterations === 'number' ? data.iterations : 0,
            budget_exhausted: !!data.budget_exhausted,
            ts: Date.now(),
          };
          history.push(entry);
          saveHistory(history);
          renderEntry(entry);
          scrollToBottom();
        })
        .catch(function (e) {
          if (pending.parentNode) messages.removeChild(pending);
          const err = document.createElement('div');
          err.className = 'chat-panel-error';
          err.textContent = 'Error: ' + (e && e.message ? e.message : 'request failed');
          messages.appendChild(err);
          scrollToBottom();
        })
        .finally(function () {
          inFlightBadge.dataset.loading = '0';
        });
    });
  }

  // ---- clear button --------------------------------------------------------
  const clearBtn = document.getElementById('chat-clear');
  if (clearBtn) {
    clearBtn.addEventListener('click', function () {
      if (!confirm('Clear this conversation? This only affects your browser.')) return;
      history.length = 0;
      try { localStorage.removeItem(STORAGE_KEY); } catch (_e) { /* ignore */ }
      messages.innerHTML = '';
    });
  }

  // ---- resize drag handle --------------------------------------------------
  let dragging = false;
  let startX = 0;
  let startWidth = 0;

  handle.addEventListener('mousedown', function (ev) {
    dragging = true;
    startX = ev.clientX;
    startWidth = panel.offsetWidth;
    document.body.style.userSelect = 'none';
    ev.preventDefault();
  });

  document.addEventListener('mousemove', function (ev) {
    if (!dragging) return;
    const delta = startX - ev.clientX; // drag left = grow
    let next = startWidth + delta;
    if (next < MIN_WIDTH) next = MIN_WIDTH;
    if (next > MAX_WIDTH) next = MAX_WIDTH;
    panel.style.width = next + 'px';
    // Update the CSS variable, not the inline padding-right — same
    // rationale as the hydration block above. The CSS rule that
    // reads `var(--chat-panel-width)` is gated on `.chat-open` +
    // xl viewport, so the body only reserves space when it should.
    document.body.style.setProperty('--chat-panel-width', next + 'px');
  });

  document.addEventListener('mouseup', function () {
    if (!dragging) return;
    dragging = false;
    document.body.style.userSelect = '';
    try {
      localStorage.setItem(WIDTH_KEY, panel.offsetWidth.toString());
    } catch (_e) { /* ignore */ }
  });
})();
