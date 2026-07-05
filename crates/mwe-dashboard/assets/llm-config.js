// llm-config.js — client logic for the admin LLM page (/admin/llm-config).
// Loaded with `defer`. Reads the inline JSON blob `#llm-config-data`
// (models.dev catalog metadata + per-provider usable-auth status) and wires:
//
//   1. Per-role provider <select> → swap the model <input>'s datalist and
//      refresh the metadata strip + the "needs a key" warning.
//   2. Per-role model <input> → refresh the metadata strip.
//   3. The Anthropic auth-mode radios → reveal the API-key block or the
//      Claude Code login block.
//
// The Advanced knobs use a native <details>, so they need no JS. Without
// JS the page still works: provider/model are plain inputs, the datalists
// are static, and the credential forms post on their own.

(function () {
  'use strict';

  const dataEl = document.getElementById('llm-config-data');
  if (!dataEl) return;
  let DATA;
  try {
    DATA = JSON.parse(dataEl.textContent);
  } catch (e) {
    return;
  }
  const catalog = DATA.catalog || {};
  const auth = DATA.auth || {};

  function fmtTokens(n) {
    if (!n) return null;
    return n >= 1000 ? Math.round(n / 1000) + 'k' : String(n);
  }

  function metaFor(provider, modelId) {
    const list = catalog[provider] || [];
    const m = list.find((x) => x.id === modelId);
    if (!m) return '';
    const bits = [];
    const ctx = fmtTokens(m.context);
    if (ctx) bits.push(ctx + ' ctx');
    if (m.input_cost != null && m.output_cost != null) {
      bits.push('$' + m.input_cost + '/$' + m.output_cost + ' per M');
    }
    if (m.vision) bits.push('vision');
    if (m.tools) bits.push('tools');
    if (m.reasoning) bits.push('reasoning');
    return bits.join(' · ');
  }

  function fmtBytes(n) {
    if (!n) return '';
    const gb = n / 1e9;
    return gb >= 1 ? gb.toFixed(1) + ' GB' : Math.round(n / 1e6) + ' MB';
  }

  // Ollama's installed models aren't in the models.dev catalog. Fetch the
  // daemon's tags once (honouring the provider-level endpoint + Bearer set
  // server-side) and fill the shared #models-ollama datalist so the model
  // field offers exact names. Best-effort, short timeout: on any failure the
  // field stays free-text.
  let ollamaModelsPromise = null;
  function ensureOllamaModels() {
    if (ollamaModelsPromise) return ollamaModelsPromise;
    const dl = document.getElementById('models-ollama');
    if (!dl) return Promise.resolve();
    const ctrl = new AbortController();
    const timer = setTimeout(() => ctrl.abort(), 4000);
    ollamaModelsPromise = fetch('/dashboard/admin/ollama-models', {
      headers: { Accept: 'application/json' },
      signal: ctrl.signal,
    })
      .then((r) => (r.ok ? r.json() : { models: [] }))
      .then((d) => {
        const models = (d && d.models) || [];
        dl.innerHTML = '';
        for (const m of models) {
          const opt = document.createElement('option');
          opt.value = m.name;
          if (m.size) opt.label = m.name + ' · ' + fmtBytes(m.size);
          dl.appendChild(opt);
        }
      })
      .catch(() => {})
      .finally(() => clearTimeout(timer));
    return ollamaModelsPromise;
  }

  function refreshRow(row) {
    const sel = row.querySelector('[data-role-provider]');
    const input = row.querySelector('[data-role-model]');
    const meta = row.querySelector('[data-role-meta]');
    const warn = row.querySelector('[data-role-warn]');
    if (!sel || !input) return;
    const provider = sel.value;
    if (catalog[provider]) {
      input.setAttribute('list', 'models-' + provider);
    } else if (provider === 'ollama') {
      input.setAttribute('list', 'models-ollama');
      ensureOllamaModels();
    } else {
      input.removeAttribute('list');
    }
    if (meta) meta.textContent = metaFor(provider, input.value.trim());
    if (warn) {
      const needsKey = provider in auth && auth[provider] === false;
      warn.style.display = needsKey ? '' : 'none';
    }
  }

  for (const row of document.querySelectorAll('[data-role-row]')) {
    const sel = row.querySelector('[data-role-provider]');
    const input = row.querySelector('[data-role-model]');
    if (sel) sel.addEventListener('change', () => refreshRow(row));
    if (input) input.addEventListener('input', () => refreshRow(row));
    refreshRow(row);
  }

  // ---- Anthropic auth-mode toggle (API key vs Claude Code login) ----------
  const modeRadios = document.querySelectorAll('input[name="anthropic_auth_mode"]');
  function applyMode() {
    let mode = 'key';
    for (const r of modeRadios) if (r.checked) mode = r.value;
    const keyBlock = document.getElementById('anthropic-key-block');
    const loginBlock = document.getElementById('anthropic-login-block');
    if (keyBlock) keyBlock.style.display = mode === 'key' ? '' : 'none';
    if (loginBlock) loginBlock.style.display = mode === 'login' ? '' : 'none';
  }
  for (const r of modeRadios) r.addEventListener('change', applyMode);
  if (modeRadios.length) applyMode();
})();
