/* Recall-trace 3D replay — the route of one recall run, told in the order
 * the engine walks it.
 *
 * Mounted on #trace-stage by the Traces viewer page
 * (/dashboard/recall-traces/:id). Fetches the trace JSON from the sibling
 * /data endpoint and replays it as a guided scene. Every beat below reads a
 * field the record (mwe_core::recall_trace::RecallTrace, payload version 2)
 * actually carries; nothing on the stage is invented:
 *
 *   1. the turn — who asked, what the engine read it as, how deep it was
 *      allowed to look; when the classifier completed the sentence, the
 *      sentence rewrites itself in front of the viewer;
 *   2. the memory answers — the facts the search returned, each with the
 *      seat it took in the block (similarity, the macrotopic quota, the
 *      one-per-kind seat) and the link key when that is what matched; the
 *      fresh captures drift above the pages because they have none; the
 *      dated items tick on a clock; the project notes stand aside;
 *   3. handed over whole — the identity cards served to the consumer, and
 *      therefore closed to the walk;
 *   4. the doors — the pages the walk may start from, on four rails
 *      (similarity, the page's own description, topic, situation), each lit
 *      by the fact that opened it where a fact did;
 *   5. the walk — the navigator (an orb with an eye) is shown a pool of
 *      candidate cards each step, picks, is vetted (every refusal has its
 *      reason), reads (the prose streams in, the budget drains), and the
 *      pages it opened sprout their links as new cards; a light turn puts
 *      the navigator to sleep instead;
 *   6. the stop — why the walk ended;
 *   7. the block — what the consumer received, assembled band by band from
 *      the sources above, with the facts that were dropped falling away and
 *      saying why; and the time the recall took inside the whole turn.
 *
 * A `wiki_navigate` trace is the same walk without a conversation around it:
 * no intent, no fresh captures, no clock, and an answer instead of a block.
 *
 * Degrades silently: no WebGL / no data → the stage collapses and the textual
 * trace below remains the whole surface. Reduced motion starts paused.
 *
 * ES module by design — it imports the vendored Three.js (pinned, MIT,
 * three.LICENSE.txt) relative to its own URL, so everything stays
 * self-hosted (air-gapped friendly, no CDN).
 */

import * as THREE from './three.module.min.js';

(async function main() {
  'use strict';

  const stage = document.getElementById('trace-stage');
  if (!stage) return;

  // ---------- data ----------

  let data;
  try {
    const resp = await fetch(`${window.location.pathname}/data`, {
      headers: { Accept: 'application/json' },
      credentials: 'same-origin',
    });
    if (!resp.ok) throw new Error(`data endpoint: ${resp.status}`);
    data = await resp.json();
  } catch (err) {
    console.warn('recall-trace: no data, stage disabled', err);
    stage.remove();
    return;
  }
  const trace = data.trace || {};
  const NO_PAGE = '(no page named)';
  const pageOf = (p) => p || NO_PAGE;
  const keyOf = (wiki, page) => `${wiki}//${pageOf(page)}`;
  const isNavigate = (trace.producer || data.source) === 'navigate';
  const isLight = trace.recall_depth === 'light';

  // ---------- palette (the dashboard's phosphor tokens) ----------

  const css = getComputedStyle(document.documentElement);
  const tok = (name, fallback) => (css.getPropertyValue(name) || '').trim() || fallback;
  const PALETTE = {
    bg: tok('--bg', '#060a09'),
    phosphor: tok('--p', '#34e1a8'),
    bright: tok('--p-bright', '#8affd2'),
    dim: tok('--p-dim', '#1f7f5f'),
    text: tok('--text', '#cfeee0'),
    textDim: tok('--text-dim', '#6fae91'),
    amber: tok('--amber', '#ffc857'),
    sky: tok('--sky', '#6fd3ff'),
    rose: tok('--rose', '#ff6f91'),
  };
  // Door families — the origins the gatherer and the walk produce today.
  const FAMILY = {
    rag: PALETTE.bright,
    description: PALETTE.phosphor,
    topic: PALETTE.sky,
    situational: PALETTE.rose,
    link: PALETTE.dim,
    card: PALETTE.amber,
  };
  const FAMILY_WORDS = {
    rag: 'similarity',
    description: 'the page says so',
    topic: 'topic',
    situational: 'situation',
    link: 'a link',
    card: 'a card rail',
  };
  // The seat a flat hit took in the block.
  const SEAT = {
    similarity: { word: 'similarity', color: PALETTE.bright },
    macrotopic_quota: { word: 'quota seat', color: PALETTE.sky },
    one_fact_per_kind: { word: 'one per kind', color: PALETTE.amber },
  };
  const DROP_WORDS = {
    rules_page: 'a rules page — never recalled as a fact',
    on_an_injected_page: 'the page it sits on was read whole by the walk',
    relevance_floor: 'below the relevance floor',
    intent_skipped_the_slot: 'the turn did not open the facts slot',
  };
  const REFUSAL_WORDS = {
    not_offered: 'not among the cards it was shown',
    wiki_vanished: 'the wiki is gone',
    rules_page: 'a rules page — closed to the walk',
    already_read: 'asked twice in one breath',
    acl_unreadable: 'this reader may not open it',
    unreadable: 'the page could not be read',
  };
  // Same seven sentences as `nav_stop_sentence` in the viewer's Rust.
  const STOP_SENTENCES = {
    done: 'the navigator judged what it had collected enough',
    budget: 'the prose budget ran out',
    hop_cap: 'the walk reached its depth limit',
    llm_degraded: 'the navigator model failed — the turn went on with what was collected',
    nothing_opened: 'every page it asked for was refused',
    pool_exhausted: 'no unvisited candidate was left',
    empty_fan: 'there was no door to start from',
  };
  const SKIP_SENTENCE = isLight
    ? 'the turn asked for a light recall — the walk was skipped by request'
    : 'the walk did not run this turn';

  // ---------- renderer / scene ----------

  let renderer;
  try {
    renderer = new THREE.WebGLRenderer({ antialias: true, alpha: false });
  } catch (err) {
    console.warn('recall-trace: WebGL unavailable, stage disabled', err);
    stage.remove();
    return;
  }
  renderer.setClearColor(new THREE.Color(PALETTE.bg), 1);
  renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, 2));
  stage.appendChild(renderer.domElement);

  const scene = new THREE.Scene();
  scene.fog = new THREE.Fog(new THREE.Color(PALETTE.bg), 26, 48);
  const camera = new THREE.PerspectiveCamera(50, 16 / 9, 0.1, 100);

  function resize() {
    const w = stage.clientWidth || 800;
    const h = stage.clientHeight || 480;
    renderer.setSize(w, h, false);
    camera.aspect = w / h;
    camera.updateProjectionMatrix();
  }
  window.addEventListener('resize', resize);

  // The dark of the memory: a slow field of motes, each one a fact nobody
  // asked for this turn.
  const field = (() => {
    const N = 420;
    const pos = new Float32Array(N * 3);
    for (let i = 0; i < N; i++) {
      pos[i * 3] = (Math.random() - 0.5) * 84;
      pos[i * 3 + 1] = (Math.random() - 0.5) * 52;
      pos[i * 3 + 2] = -6 - Math.random() * 40;
    }
    const geo = new THREE.BufferGeometry();
    geo.setAttribute('position', new THREE.BufferAttribute(pos, 3));
    const mat = new THREE.PointsMaterial({
      color: new THREE.Color(PALETTE.dim),
      size: 0.06,
      transparent: true,
      opacity: 0.5,
      depthWrite: false,
    });
    const pts = new THREE.Points(geo, mat);
    scene.add(pts);
    return pts;
  })();

  // ---------- tiny helpers ----------

  const clamp01 = (x) => Math.min(1, Math.max(0, x));
  const easeInOut = (t) => (t < 0.5 ? 4 * t * t * t : 1 - Math.pow(-2 * t + 2, 3) / 2);
  const easeOut = (t) => 1 - Math.pow(1 - t, 3);
  const lerp = (a, b, t) => a + (b - a) * t;
  const fmt = (n) => new Intl.NumberFormat('en-US').format(n);
  const esc = (s) =>
    String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c]);
  const plural = (n, one, many) => `${fmt(n)} ${n === 1 ? one : many}`;

  function radialSprite(color, inner = 0.18) {
    const c = document.createElement('canvas');
    c.width = c.height = 128;
    const g = c.getContext('2d');
    const grad = g.createRadialGradient(64, 64, 64 * inner, 64, 64, 64);
    grad.addColorStop(0, color);
    grad.addColorStop(1, 'rgba(0,0,0,0)');
    g.fillStyle = grad;
    g.fillRect(0, 0, 128, 128);
    const tex = new THREE.CanvasTexture(c);
    return new THREE.Sprite(
      new THREE.SpriteMaterial({
        map: tex,
        blending: THREE.AdditiveBlending,
        transparent: true,
        depthWrite: false,
      }),
    );
  }

  const MONO = '"JetBrains Mono", ui-monospace, monospace';

  function wrapMono(g, text, maxW) {
    const out = [];
    for (const hard of String(text).split(/\n/)) {
      let line = '';
      for (const word of hard.split(/\s+/)) {
        const probe = line ? `${line} ${word}` : word;
        if (g.measureText(probe).width <= maxW || !line) line = probe;
        else {
          out.push(line);
          line = word;
        }
      }
      out.push(line);
    }
    return out;
  }

  function chip(g, x, y, label, color) {
    g.font = `13px ${MONO}`;
    const w = g.measureText(label).width + 14;
    g.strokeStyle = color;
    g.fillStyle = color;
    g.lineWidth = 1;
    g.strokeRect(x, y, w, 20);
    g.fillText(label, x + 7, y + 15);
    return w + 8;
  }

  /** Anything that floats, chases a pose and can be billboarded. */
  class Actor {
    constructor(group) {
      this.group = group;
      this.group.visible = false;
      this.group.scale.setScalar(0.001);
      this.target = { pos: new THREE.Vector3(), scale: 0.001, opacity: 0, halo: 0 };
      this.opacity = 0;
      this.haloOpacity = 0;
      this.spin = 0;
      scene.add(group);
    }
    applyOpacity() {}
    tween(dt, rate = 4.2) {
      const k = clamp01(dt * rate);
      this.group.position.lerp(this.target.pos, k);
      const s = lerp(this.group.scale.x, this.target.scale, k);
      this.group.scale.setScalar(s);
      this.opacity = lerp(this.opacity, this.target.opacity, k);
      this.haloOpacity = lerp(this.haloOpacity, this.target.halo, k);
      this.applyOpacity();
      this.group.visible = this.opacity > 0.01;
      if (this.group.visible) this.group.lookAt(camera.position);
    }
    hide() {
      this.target.opacity = 0;
      this.target.scale = 0.001;
      this.target.halo = 0;
    }
    place(pos, scale, opacity, halo) {
      this.target.pos.copy(pos);
      this.target.scale = scale;
      this.target.opacity = opacity;
      this.target.halo = halo;
    }
  }

  // ---------- page cards ----------

  const CARD_W = 2.6;
  const CARD_H = 3.3;
  const TEX_W = 416;
  const TEX_H = 528;

  /** One wiki page. Redraws its own canvas texture on demand. */
  class Card extends Actor {
    constructor({ wiki, page, accent }) {
      const group = new THREE.Group();
      super(group);
      this.wiki = wiki;
      this.page = pageOf(page);
      this.accent = accent || PALETTE.dim;
      this.chips = []; // [{label, color}]
      this.bodyText = '';
      this.highlights = []; // [{text, score, seat, linkKey}]
      this.streamChars = -1; // -1 = no streaming: draw full body
      this.door = null; // origin, when the page is a door of the walk
      this.closed = false; // served whole → closed to the walk
      this.torn = false; // the budget cut this page
      this.read = false;

      this.canvas = document.createElement('canvas');
      this.canvas.width = TEX_W;
      this.canvas.height = TEX_H;
      this.tex = new THREE.CanvasTexture(this.canvas);
      this.tex.anisotropy = 4;
      const mat = new THREE.MeshBasicMaterial({
        map: this.tex,
        transparent: true,
        opacity: 0,
        side: THREE.DoubleSide,
      });
      this.mesh = new THREE.Mesh(new THREE.PlaneGeometry(CARD_W, CARD_H), mat);
      this.mesh.userData.card = this;
      // Kept tight and faint — the halo is an accent, the card text must
      // stay readable.
      this.halo = radialSprite(this.accent, 0.05);
      this.halo.scale.set(CARD_W * 1.55, CARD_H * 1.3, 1);
      this.halo.material.opacity = 0;
      this.mesh.add(this.halo);
      group.add(this.mesh);
      this.draw();
    }

    applyOpacity() {
      this.mesh.material.opacity = this.opacity;
      this.halo.material.opacity = this.haloOpacity;
    }

    setAccent(color) {
      this.accent = color;
      const spr = radialSprite(color, 0.05);
      this.halo.material.map = spr.material.map;
      this.halo.material.needsUpdate = true;
      spr.material.dispose();
    }

    addChip(label, color) {
      if (!this.chips.some((c) => c.label === label)) this.chips.push({ label, color });
    }

    /** Repaint the card texture (frame, header, chips, door, body). */
    draw() {
      const g = this.canvas.getContext('2d');
      const W = TEX_W;
      const H = TEX_H;
      g.clearRect(0, 0, W, H);
      // Panel — torn along the bottom when the budget cut it.
      g.fillStyle = 'rgba(6, 14, 11, 0.93)';
      g.strokeStyle = this.accent;
      g.lineWidth = 2;
      g.beginPath();
      if (this.torn) {
        g.moveTo(11, 1);
        g.lineTo(W - 11, 1);
        g.quadraticCurveTo(W - 1, 1, W - 1, 11);
        g.lineTo(W - 1, H - 40);
        for (let x = W - 1; x > 1; x -= 26) g.lineTo(x - 13, H - 40 + (x % 52 ? 22 : 6));
        g.lineTo(1, H - 40);
        g.lineTo(1, 11);
        g.quadraticCurveTo(1, 1, 11, 1);
        g.closePath();
      } else {
        g.roundRect(1, 1, W - 2, H - 2, 10);
      }
      g.fill();
      g.stroke();
      // Corner ticks (the dashboard's term-panel look)
      g.lineWidth = 3;
      for (const [x, y, dx, dy] of [
        [8, 8, 1, 1],
        [W - 8, 8, -1, 1],
        [8, H - 8, 1, -1],
        [W - 8, H - 8, -1, -1],
      ]) {
        if (this.torn && dy < 0) continue;
        g.beginPath();
        g.moveTo(x + dx * 14, y);
        g.lineTo(x, y);
        g.lineTo(x, y + dy * 14);
        g.stroke();
      }
      // Header: wiki id + page
      g.fillStyle = PALETTE.bright;
      g.font = `bold 22px ${MONO}`;
      g.fillText(this.wiki, 20, 40, W - 96);
      g.fillStyle = this.page === NO_PAGE ? PALETTE.textDim : PALETTE.text;
      g.font = `16px ${MONO}`;
      g.fillText(this.page, 20, 64, W - 96);
      // The door: a frame with a knob, in the family colour, top right.
      if (this.door) {
        const col = FAMILY[this.door] || PALETTE.dim;
        g.strokeStyle = col;
        g.lineWidth = 2;
        g.strokeRect(W - 66, 22, 40, 56);
        g.fillStyle = col;
        g.beginPath();
        g.arc(W - 36, 52, 3.5, 0, Math.PI * 2);
        g.fill();
        if (this.read) {
          // The door stands ajar once the walk went through it.
          g.beginPath();
          g.moveTo(W - 66, 22);
          g.lineTo(W - 52, 30);
          g.lineTo(W - 52, 86);
          g.lineTo(W - 66, 78);
          g.closePath();
          g.fillStyle = 'rgba(6,14,11,0.95)';
          g.fill();
          g.stroke();
        }
      }
      if (this.closed) {
        // Served whole: a barred door.
        g.strokeStyle = PALETTE.amber;
        g.lineWidth = 2;
        g.strokeRect(W - 66, 22, 40, 56);
        g.beginPath();
        g.moveTo(W - 72, 44);
        g.lineTo(W - 20, 56);
        g.moveTo(W - 72, 60);
        g.lineTo(W - 20, 72);
        g.stroke();
      }
      // Chips
      let cx = 20;
      for (const c of this.chips.slice(0, 4)) cx += chip(g, cx, 78, c.label, c.color);
      g.strokeStyle = 'rgba(120,180,150,0.35)';
      g.lineWidth = 1;
      g.beginPath();
      g.moveTo(20, 108);
      g.lineTo(W - 20, 108);
      g.stroke();

      // Highlighted (matched) regions — the glow of the search.
      let y = 134;
      g.font = `15px ${MONO}`;
      for (const h of this.highlights.slice(0, 2)) {
        const col = (h.seat && SEAT[h.seat]?.color) || this.accent;
        const lines = wrapMono(g, h.text, W - 56).slice(0, 4);
        for (const line of lines) {
          if (y > H - 26) break;
          g.save();
          g.shadowColor = col;
          g.shadowBlur = 8;
          g.fillStyle = 'rgba(52,225,168,0.10)';
          g.fillRect(16, y - 15, W - 32, 21);
          g.restore();
          g.fillStyle = PALETTE.bright;
          g.fillText(line, 24, y, W - 48);
          y += 22;
        }
        g.fillStyle = PALETTE.textDim;
        g.font = `12px ${MONO}`;
        const meta = [];
        if (h.score != null) meta.push(`${h.linkKey ? 'link key' : 'score'} ${h.score.toFixed(3)}`);
        if (h.voted != null) meta.push(`voted ${h.voted.toFixed(3)}`);
        if (h.seat && SEAT[h.seat]) meta.push(SEAT[h.seat].word);
        if (h.kind) meta.push(h.kind);
        if (meta.length) g.fillText(meta.join(' · '), 24, y);
        g.font = `15px ${MONO}`;
        y += 24;
      }

      // Body prose (streams during the reading phase).
      if (this.bodyText) {
        const shown =
          this.streamChars >= 0 ? this.bodyText.slice(0, this.streamChars) : this.bodyText;
        g.fillStyle = PALETTE.text;
        g.font = `14px ${MONO}`;
        const lines = wrapMono(g, shown, W - 48);
        const floor = this.torn ? H - 52 : H - 22;
        for (const line of lines) {
          if (y > floor) break;
          g.fillText(line, 24, y, W - 48);
          y += 19;
        }
        if (this.streamChars >= 0 && this.streamChars < this.bodyText.length && y <= floor) {
          g.fillStyle = PALETTE.bright;
          g.fillRect(24 + g.measureText(lines[lines.length - 1] || '').width + 3, y - 33, 9, 16);
        }
      }
      this.tex.needsUpdate = true;
    }
  }

  // ---------- motes: fresh captures, which have no page ----------

  const MOTE_W = 300;
  const MOTE_H = 96;

  class Mote extends Actor {
    constructor(text, kind) {
      const group = new THREE.Group();
      super(group);
      this.text = text;
      const c = document.createElement('canvas');
      c.width = MOTE_W;
      c.height = MOTE_H;
      const g = c.getContext('2d');
      g.fillStyle = 'rgba(6,14,11,0.85)';
      g.strokeStyle = PALETTE.phosphor;
      g.lineWidth = 1.5;
      g.beginPath();
      g.roundRect(1, 1, MOTE_W - 2, MOTE_H - 2, 24);
      g.fill();
      g.stroke();
      g.fillStyle = PALETTE.text;
      g.font = `14px ${MONO}`;
      const lines = wrapMono(g, text, MOTE_W - 40).slice(0, 3);
      let y = 34;
      for (const line of lines) {
        g.fillText(line, 20, y, MOTE_W - 40);
        y += 19;
      }
      g.fillStyle = PALETTE.textDim;
      g.font = `11px ${MONO}`;
      g.fillText(`not yet on a page${kind ? ' · ' + kind : ''}`, 20, MOTE_H - 12);
      this.tex = new THREE.CanvasTexture(c);
      const mat = new THREE.MeshBasicMaterial({ map: this.tex, transparent: true, opacity: 0 });
      this.mesh = new THREE.Mesh(new THREE.PlaneGeometry(2.2, 2.2 * (MOTE_H / MOTE_W)), mat);
      this.halo = radialSprite(PALETTE.phosphor, 0.02);
      this.halo.scale.set(2.8, 1.4, 1);
      this.halo.material.opacity = 0;
      this.mesh.add(this.halo);
      group.add(this.mesh);
      this.drift = Math.random() * Math.PI * 2;
    }
    applyOpacity() {
      this.mesh.material.opacity = this.opacity;
      this.halo.material.opacity = this.haloOpacity;
    }
    tween(dt) {
      this.drift += dt * 0.8;
      super.tween(dt, 3.4);
      if (this.group.visible) this.group.position.y += Math.sin(this.drift) * 0.004;
    }
  }

  // ---------- the clock: dated items closing soon ----------

  class Clock extends Actor {
    constructor(items) {
      const group = new THREE.Group();
      super(group);
      this.items = items;
      const S = 320;
      const c = document.createElement('canvas');
      c.width = S;
      c.height = S + 120;
      this.canvas = c;
      this.tex = new THREE.CanvasTexture(c);
      const mat = new THREE.MeshBasicMaterial({ map: this.tex, transparent: true, opacity: 0 });
      this.mesh = new THREE.Mesh(new THREE.PlaneGeometry(2.4, 2.4 * ((S + 120) / S)), mat);
      this.halo = radialSprite(PALETTE.amber, 0.05);
      this.halo.scale.set(3.4, 3.4, 1);
      this.halo.material.opacity = 0;
      this.mesh.add(this.halo);
      group.add(this.mesh);
      this.hand = 0;
      this.draw();
    }
    applyOpacity() {
      this.mesh.material.opacity = this.opacity;
      this.halo.material.opacity = this.haloOpacity;
    }
    draw() {
      const g = this.canvas.getContext('2d');
      const S = 320;
      g.clearRect(0, 0, S, S + 120);
      g.strokeStyle = PALETTE.amber;
      g.fillStyle = 'rgba(6,14,11,0.9)';
      g.lineWidth = 3;
      g.beginPath();
      g.arc(S / 2, S / 2, S / 2 - 6, 0, Math.PI * 2);
      g.fill();
      g.stroke();
      for (let i = 0; i < 12; i++) {
        const a = (i / 12) * Math.PI * 2;
        g.beginPath();
        g.moveTo(S / 2 + Math.cos(a) * (S / 2 - 16), S / 2 + Math.sin(a) * (S / 2 - 16));
        g.lineTo(S / 2 + Math.cos(a) * (S / 2 - 28), S / 2 + Math.sin(a) * (S / 2 - 28));
        g.stroke();
      }
      // Hands: the minute hand sweeps as the beat plays.
      const a = -Math.PI / 2 + this.hand * Math.PI * 2;
      g.lineWidth = 4;
      g.beginPath();
      g.moveTo(S / 2, S / 2);
      g.lineTo(S / 2 + Math.cos(a) * (S / 2 - 50), S / 2 + Math.sin(a) * (S / 2 - 50));
      g.stroke();
      g.lineWidth = 6;
      const h = -Math.PI / 2 + (this.hand / 12) * Math.PI * 2 + 0.9;
      g.beginPath();
      g.moveTo(S / 2, S / 2);
      g.lineTo(S / 2 + Math.cos(h) * (S / 2 - 100), S / 2 + Math.sin(h) * (S / 2 - 100));
      g.stroke();
      // The items, with their dates — the one thing that put them here.
      g.fillStyle = PALETTE.text;
      g.font = `14px ${MONO}`;
      let y = S + 22;
      for (const it of this.items.slice(0, 3)) {
        const when = it.valid_to ? String(it.valid_to).replace('T', ' ').slice(0, 16) : '';
        const line = wrapMono(g, `${when} — ${it.text}`, S - 24)[0] || '';
        g.fillText(line, 12, y, S - 24);
        y += 20;
      }
      if (this.items.length > 3) {
        g.fillStyle = PALETTE.textDim;
        g.fillText(`… ${this.items.length - 3} more`, 12, y);
      }
      this.tex.needsUpdate = true;
    }
  }

  // ---------- the block: what the consumer received ----------

  // Section headers the engine writes into the block, in its own words.
  const BANDS = [
    { key: 'you', head: 'WHO YOU ARE', word: 'the assistant’s own card', color: PALETTE.amber },
    { key: 'speaker', head: 'WHO IS SPEAKING', word: 'the speaker’s card', color: PALETTE.amber },
    { key: 'about', head: 'PEOPLE THIS TURN IS ABOUT', word: 'the people named', color: PALETTE.amber },
    { key: 'history', head: 'YOUR RECENT HISTORY', word: 'recent history', color: PALETTE.textDim },
    { key: 'facts', head: 'RELEVANT MEMORY', word: 'recalled facts', color: PALETTE.bright },
    { key: 'fresh', head: 'Recent (not yet consolidated)', word: 'fresh captures', color: PALETTE.phosphor },
    { key: 'docs', head: 'Project documentation', word: 'project notes', color: PALETTE.sky },
    { key: 'walk', head: 'NAVIGATED PAGES', word: 'pages the walk read', color: PALETTE.bright },
    { key: 'clock', head: 'UPCOMING', word: 'closing soon', color: PALETTE.amber },
  ];

  /** Split the injected block into its bands, by the headers it carries. */
  function bandsOf(block) {
    const found = [];
    for (const b of BANDS) {
      const i = block.indexOf(b.head);
      if (i >= 0) found.push({ ...b, at: i });
    }
    found.sort((a, b) => a.at - b.at);
    for (let i = 0; i < found.length; i++) {
      const end = i + 1 < found.length ? found[i + 1].at : block.length;
      found[i].chars = end - found[i].at;
    }
    return found;
  }

  class Block extends Actor {
    constructor(bands, totalChars) {
      const group = new THREE.Group();
      super(group);
      this.bands = bands;
      this.total = Math.max(totalChars, 1);
      this.fill = 0; // 0..1, the bands light up top to bottom
      this.W = 360;
      this.H = 560;
      const c = document.createElement('canvas');
      c.width = this.W;
      c.height = this.H;
      this.canvas = c;
      this.tex = new THREE.CanvasTexture(c);
      const mat = new THREE.MeshBasicMaterial({ map: this.tex, transparent: true, opacity: 0 });
      this.mesh = new THREE.Mesh(new THREE.PlaneGeometry(2.9, 2.9 * (this.H / this.W)), mat);
      this.halo = radialSprite(PALETTE.bright, 0.05);
      this.halo.scale.set(4.2, 5.6, 1);
      this.halo.material.opacity = 0;
      this.mesh.add(this.halo);
      group.add(this.mesh);
      this.draw();
    }
    applyOpacity() {
      this.mesh.material.opacity = this.opacity;
      this.halo.material.opacity = this.haloOpacity;
    }
    /** Band heights: proportional to their characters, never under a floor,
     * and always adding up to the panel — the first pass hands out floors,
     * the second shares what is left by size. */
    heights() {
      const usable = this.H - 60;
      const floor = 24;
      const flex = this.bands.filter((b) => usable * (b.chars / this.total) > floor);
      const floored = this.bands.length - flex.length;
      const left = usable - floored * floor;
      const flexTotal = flex.reduce((n, b) => n + b.chars, 0) || 1;
      return this.bands.map((b) => (flex.includes(b) ? Math.max(floor, left * (b.chars / flexTotal)) : floor));
    }
    /** Where (in card space) a band's centre sits — for the flights. */
    bandCentre(key) {
      let y = 0;
      const hs = this.heights();
      for (let i = 0; i < this.bands.length; i++) {
        const h = hs[i];
        if (this.bands[i].key === key) return (0.5 - (48 + y + h / 2) / this.H) * 2.9 * (this.H / this.W);
        y += h;
      }
      return 0;
    }
    draw() {
      const g = this.canvas.getContext('2d');
      const { W, H } = this;
      g.clearRect(0, 0, W, H);
      g.fillStyle = 'rgba(6,14,11,0.94)';
      g.strokeStyle = PALETTE.bright;
      g.lineWidth = 2;
      g.beginPath();
      g.roundRect(1, 1, W - 2, H - 2, 10);
      g.fill();
      g.stroke();
      g.fillStyle = PALETTE.bright;
      g.font = `bold 18px ${MONO}`;
      g.fillText(isNavigate ? 'the answer' : 'the block', 18, 32);
      const hs = this.heights();
      let y = 48;
      let lit = this.fill * this.total;
      for (let i = 0; i < this.bands.length; i++) {
        const b = this.bands[i];
        const h = hs[i];
        const on = clamp01(lit / b.chars);
        lit -= b.chars;
        g.fillStyle = `rgba(52,225,168,${0.05 + 0.18 * on})`;
        g.fillRect(12, y, W - 24, h - 3);
        g.strokeStyle = b.color;
        g.globalAlpha = 0.35 + 0.65 * on;
        g.lineWidth = 2;
        g.strokeRect(12, y, W - 24, h - 3);
        g.globalAlpha = 1;
        g.fillStyle = on > 0.3 ? PALETTE.text : PALETTE.textDim;
        g.font = `13px ${MONO}`;
        g.fillText(`${b.word} · ${fmt(b.chars)}`, 20, y + 16, W - 40);
        y += h;
      }
      this.tex.needsUpdate = true;
    }
  }

  // ---------- the navigator orb ----------

  function eyeTexture(closed) {
    const c = document.createElement('canvas');
    c.width = c.height = 128;
    const g = c.getContext('2d');
    if (closed) {
      g.strokeStyle = '#0b3d27';
      g.lineWidth = 9;
      g.lineCap = 'round';
      g.beginPath();
      g.arc(64, 44, 40, 0.25 * Math.PI, 0.75 * Math.PI);
      g.stroke();
      return new THREE.CanvasTexture(c);
    }
    g.fillStyle = '#f4fff9';
    g.beginPath();
    g.arc(64, 64, 44, 0, Math.PI * 2);
    g.fill();
    g.fillStyle = '#0b3d27';
    g.beginPath();
    g.arc(64, 64, 26, 0, Math.PI * 2);
    g.fill();
    g.fillStyle = '#02120a';
    g.beginPath();
    g.arc(64, 64, 13, 0, Math.PI * 2);
    g.fill();
    g.fillStyle = 'rgba(255,255,255,0.85)';
    g.beginPath();
    g.arc(53, 52, 6, 0, Math.PI * 2);
    g.fill();
    return new THREE.CanvasTexture(c);
  }

  function makeOrb() {
    const group = new THREE.Group();
    const body = new THREE.Mesh(
      new THREE.SphereGeometry(0.42, 32, 32),
      new THREE.MeshBasicMaterial({ color: new THREE.Color(PALETTE.phosphor) }),
    );
    const openEye = eyeTexture(false);
    const closedEye = eyeTexture(true);
    const eye = new THREE.Sprite(new THREE.SpriteMaterial({ map: openEye, depthWrite: false }));
    eye.scale.setScalar(0.5);
    eye.position.set(0, 0.05, 0.4);
    const halo = radialSprite(PALETTE.bright, 0.1);
    halo.scale.setScalar(2.1);
    halo.material.opacity = 0.5;
    group.add(halo, body, eye);
    group.visible = false;
    scene.add(group);
    return {
      group,
      halo,
      target: new THREE.Vector3(),
      breathe: 0,
      asleep: false,
      sleep(on) {
        this.asleep = on;
        eye.material.map = on ? closedEye : openEye;
        eye.material.needsUpdate = true;
      },
    };
  }
  const orb = makeOrb();

  // Fading trail behind the orb.
  const trail = [];
  function spawnTrailDot() {
    const s = radialSprite(PALETTE.phosphor, 0.05);
    s.scale.setScalar(0.5);
    s.position.copy(orb.group.position);
    s.material.opacity = 0.5;
    scene.add(s);
    trail.push(s);
  }

  // Beams: transient light lines (hit → door, orb → page).
  const beams = [];
  function beam(from, to, color, life = 0.7) {
    const geo = new THREE.BufferGeometry().setFromPoints([from.clone(), to.clone()]);
    const mat = new THREE.LineBasicMaterial({
      color: new THREE.Color(color || PALETTE.bright),
      transparent: true,
      opacity: 0.9,
      blending: THREE.AdditiveBlending,
      depthWrite: false,
    });
    const line = new THREE.Line(geo, mat);
    line.userData.decay = life;
    scene.add(line);
    beams.push(line);
  }

  // A ripple: the question spreading through the memory.
  const ripples = [];
  function ripple(at, color) {
    const ring = new THREE.Mesh(
      new THREE.RingGeometry(0.9, 1.0, 64),
      new THREE.MeshBasicMaterial({
        color: new THREE.Color(color || PALETTE.phosphor),
        transparent: true,
        opacity: 0.6,
        side: THREE.DoubleSide,
        blending: THREE.AdditiveBlending,
        depthWrite: false,
      }),
    );
    ring.position.copy(at);
    ring.lookAt(camera.position);
    scene.add(ring);
    ripples.push(ring);
  }

  // ---------- overlay DOM (captions, controls, budget meter) ----------

  const hud = document.createElement('div');
  hud.className = 'trace-hud';
  hud.innerHTML = `
    <div class="trace-budget" hidden>
      <div class="trace-budget-label"></div>
      <div class="trace-budget-bar"><div class="trace-budget-fill"></div></div>
    </div>
    <div class="trace-caption" aria-live="polite">
      <div class="trace-caption-phase"></div>
      <div class="trace-caption-text"></div>
    </div>
    <div class="trace-controls">
      <button type="button" data-act="restart" title="Restart">⟲</button>
      <button type="button" data-act="back" title="Previous step">◀</button>
      <button type="button" data-act="play" title="Play / pause">⏸</button>
      <button type="button" data-act="fwd" title="Next step">▶</button>
      <select data-act="speed" title="Speed">
        <option value="0.6">0.6×</option>
        <option value="1" selected>1×</option>
        <option value="1.6">1.6×</option>
        <option value="2.5">2.5×</option>
      </select>
    </div>
    <div class="trace-progress"><div class="trace-progress-fill"></div></div>`;
  stage.appendChild(hud);
  const capPhase = hud.querySelector('.trace-caption-phase');
  const capText = hud.querySelector('.trace-caption-text');
  const playBtn = hud.querySelector('[data-act="play"]');
  const progressFill = hud.querySelector('.trace-progress-fill');
  const budgetBox = hud.querySelector('.trace-budget');
  const budgetLabel = hud.querySelector('.trace-budget-label');
  const budgetFill = hud.querySelector('.trace-budget-fill');

  const charBudget = trace.char_budget || 0;
  function showBudget(collected) {
    if (!charBudget) return;
    budgetBox.hidden = false;
    const frac = clamp01(collected / charBudget);
    budgetFill.style.height = `${frac * 100}%`;
    budgetLabel.textContent = `prose budget ${fmt(Math.round(collected))} / ${fmt(charBudget)}`;
    budgetBox.classList.toggle('is-spent', frac >= 0.999);
  }

  // ---------- build the cast from the trace ----------

  const cards = new Map(); // key → Card

  function pageFromSourcePath(wiki, sourcePath) {
    // "wikis/famiglia/salute.md" → "salute.md" (best-effort: strip up to
    // and including the wiki id segment).
    const marker = `/${wiki}/`;
    const i = (sourcePath || '').lastIndexOf(marker);
    if (i >= 0) return sourcePath.slice(i + marker.length);
    const parts = (sourcePath || '').split('/');
    return parts[parts.length - 1] || null;
  }

  function ensureCard(wiki, page, accent) {
    const key = keyOf(wiki, page);
    let card = cards.get(key);
    if (!card) {
      card = new Card({ wiki, page, accent });
      cards.set(key, card);
    }
    return card;
  }

  const flatHits = trace.flat_hits || [];
  const freshHits = trace.fresh_hits || [];
  const dueSoon = trace.due_soon || [];
  const projectDocs = trace.project_docs || [];
  const servedPages = trace.served_pages || [];
  const entryPoints = trace.entry_points || [];
  const hops = trace.hops || [];
  const hitById = new Map(flatHits.map((h) => [h.fact_id, h]));

  // The facts the search returned — one card per source page.
  const hitCards = [];
  const cardOfHit = new Map();
  for (const hit of flatHits) {
    const page = pageFromSourcePath(hit.wiki_id, hit.source_path);
    const card = ensureCard(hit.wiki_id, page, FAMILY.rag);
    if (card.highlights.length < 2) {
      card.highlights.push({
        text: hit.text,
        score: hit.score,
        seat: hit.seat,
        linkKey: !!hit.link_key_win,
        voted: hit.voted_score,
        kind: hit.fact_type,
      });
    }
    if (hit.seat && SEAT[hit.seat]) card.addChip(SEAT[hit.seat].word, SEAT[hit.seat].color);
    if (!hitCards.includes(card)) hitCards.push(card);
    cardOfHit.set(hit.fact_id, card);
    card.draw();
  }
  const seatCounts = {};
  for (const h of flatHits) seatCounts[h.seat || 'similarity'] = (seatCounts[h.seat || 'similarity'] || 0) + 1;
  const linkKeyWins = flatHits.filter((h) => h.link_key_win).length;

  // Fresh captures: motes, no page to sit on.
  const motes = freshHits.map((h) => new Mote(h.text, h.fact_type));

  // The clock, when anything closes soon.
  const clock = dueSoon.length ? new Clock(dueSoon) : null;

  // Project notes: cards of their own, in sky.
  const docCards = [];
  for (const d of projectDocs) {
    const page = pageFromSourcePath(d.wiki_id, d.source_path);
    const card = ensureCard(d.wiki_id, page, PALETTE.sky);
    card.setAccent(PALETTE.sky);
    card.addChip(`project · ${d.half || 'named'}`, PALETTE.sky);
    if (card.highlights.length < 2) card.highlights.push({ text: d.text, score: d.score });
    if (!docCards.includes(card)) docCards.push(card);
    card.draw();
  }

  // The identity cards handed over whole — closed to the walk.
  const servedCards = [];
  for (const s of servedPages) {
    const card = ensureCard(s.wiki_id, s.page, PALETTE.amber);
    card.setAccent(PALETTE.amber);
    card.closed = true;
    card.addChip(s.role === 'speaker' ? 'speaker' : 'named this turn', PALETTE.amber);
    if (!servedCards.includes(card)) servedCards.push(card);
    card.draw();
  }

  // The doors, in the gatherer's order (heaviest first).
  const doorCards = [];
  for (const ep of entryPoints) {
    const card = ensureCard(ep.wiki_id, ep.page, FAMILY[ep.origin] || PALETTE.dim);
    if (!card.door) card.door = ep.origin;
    card.addChip(`door · ${FAMILY_WORDS[ep.origin] || ep.origin}`, FAMILY[ep.origin] || PALETTE.dim);
    if (!hitCards.includes(card) && !docCards.includes(card)) card.setAccent(FAMILY[ep.origin] || PALETTE.dim);
    card.weight = ep.weight;
    card.matchedFact = ep.matched_fact;
    if (!doorCards.includes(card)) doorCards.push(card);
    card.draw();
  }

  // Layouts.
  function arcSlots(n, radius = 8.4, y = 0, zBias = 4) {
    const slots = [];
    const spread = Math.min(Math.PI * 0.72, 0.32 * Math.max(n - 1, 1));
    for (let i = 0; i < n; i++) {
      const order = Math.ceil(i / 2) * (i % 2 === 1 ? 1 : -1);
      const a = n > 1 ? (order / Math.max(n - 1, 1)) * spread : 0;
      slots.push(new THREE.Vector3(Math.sin(a) * radius, y + Math.cos(order) * 0.35, -Math.cos(a) * radius + zBias));
    }
    return slots;
  }
  function ringSlots(n, radius, centre, y = 0) {
    const slots = [];
    for (let i = 0; i < n; i++) {
      const a = (i / Math.max(n, 1)) * Math.PI * 2 - Math.PI / 2;
      slots.push(new THREE.Vector3(centre.x + Math.cos(a) * radius, y + Math.sin(a) * radius * 0.42, centre.z - 1));
    }
    return slots;
  }
  // Four rails for the doors, strongest claim on top: similarity, then the
  // page's own description, then a topic word, then the situation — the
  // weight decides the place along the rail.
  const RAIL_Y = { rag: 3.0, description: 1.0, topic: -1.0, situational: -3.0 };
  function railSlot(origin, weight, index, n) {
    const y = RAIL_Y[origin] ?? -1.9;
    const x = -7.5 + (1 - clamp01(weight)) * 15 + (index - n / 2) * 0.2;
    return new THREE.Vector3(x, y, 2 - (1 - clamp01(weight)) * 2.5);
  }
  const SHELF_LEFT = new THREE.Vector3(-8.6, -2.2, 6.5); // the read pages park here
  const HANDED = new THREE.Vector3(-7.4, 2.4, 2.5); // the served cards park here

  // ---------- camera rig ----------

  const rig = {
    pos: new THREE.Vector3(0, 0.6, 13),
    look: new THREE.Vector3(0, 0, 0),
    posGoal: new THREE.Vector3(0, 0.6, 13),
    lookGoal: new THREE.Vector3(0, 0, 0),
    orbit: { yaw: 0, pitch: 0 },
  };
  function camGoal(pos, look) {
    rig.posGoal.copy(pos);
    rig.lookGoal.copy(look);
  }

  let dragging = false;
  let lastX = 0;
  let lastY = 0;
  let downX = 0;
  let downY = 0;
  renderer.domElement.addEventListener('pointerdown', (e) => {
    dragging = true;
    lastX = downX = e.clientX;
    lastY = downY = e.clientY;
  });
  window.addEventListener('pointerup', () => {
    dragging = false;
  });
  window.addEventListener('pointermove', (e) => {
    if (!dragging) return;
    rig.orbit.yaw += (e.clientX - lastX) * 0.003;
    rig.orbit.pitch += (e.clientY - lastY) * 0.002;
    lastX = e.clientX;
    lastY = e.clientY;
  });

  // ---------- timeline ----------

  /** Steps: { phase, caption (text) | html, dur, enter(), update(t) } — t ∈ [0,1]. */
  const steps = [];
  const actors = () => [...cards.values(), ...motes, ...(clock ? [clock] : []), ...(block ? [block] : [])];

  const turnText = trace.turn_text || '(empty turn)';
  const completed = trace.completed_message && trace.completed_message !== turnText ? trace.completed_message : null;
  const who = [data.sender_id, trace.consumer ? `via ${trace.consumer}` : null].filter(Boolean).join(' ');

  // Phase 1 — the turn.
  {
    const readAs = [];
    if (!isNavigate && trace.intent) readAs.push(`read as: ${trace.intent}`);
    if (isLight) readAs.push('recall: light');
    steps.push({
      phase: `${isNavigate ? 'deep search' : 'the turn'} · ${who}`,
      caption: '',
      dur: Math.min(5, 1.6 + turnText.length * 0.02),
      enter() {
        camGoal(new THREE.Vector3(0, 0.6, 13), new THREE.Vector3(0, 0, 0));
      },
      update(t) {
        const n = Math.floor(easeOut(t) * turnText.length);
        const tail = readAs.length && t > 0.85 ? `\n${readAs.join(' · ')}` : '';
        capText.textContent = `“${turnText.slice(0, n)}${n < turnText.length ? '▌' : '”'}${tail}`;
      },
    });
  }

  // Phase 1b — the completed message rewrites the sentence.
  if (completed) {
    const rawWords = new Set(turnText.toLowerCase().split(/\s+/).map((w) => w.replace(/[.,;:!?]+$/, '')));
    // Tokens of the completed sentence; the ones the person never wrote are
    // the classifier's, and they light up as they appear.
    const tokens = completed.split(/(\s+)/).map((w) => ({
      text: w,
      filled: !/^\s*$/.test(w) && !rawWords.has(w.toLowerCase().replace(/[.,;:!?]+$/, '')),
    }));
    const revealTokens = (n) => {
      let left = n;
      let out = '';
      for (const tk of tokens) {
        if (left <= 0) break;
        const part = tk.text.slice(0, left);
        left -= part.length;
        out += tk.filled ? `<mark>${esc(part)}</mark>` : esc(part);
      }
      return out;
    };
    steps.push({
      phase: 'the completed message',
      caption: '',
      dur: Math.min(6, 2.4 + completed.length * 0.02),
      enter() {
        ripple(new THREE.Vector3(0, 0, 2), PALETTE.bright);
      },
      update(t) {
        if (t < 0.25) {
          capText.textContent = `“${turnText}”`;
          return;
        }
        const k = easeInOut((t - 0.25) / 0.75);
        const n = Math.floor(k * completed.length);
        capText.innerHTML =
          `“${revealTokens(n)}${n < completed.length ? '▌' : '”'}` +
          (t > 0.9 ? `\n<span class="trace-note">the classifier filled in what was left implicit — the search below answers this sentence</span>` : '');
      },
    });
  }

  // Phase 2 — the memory answers.
  {
    const parts = [];
    if (flatHits.length) {
      const seatWords = Object.entries(seatCounts)
        .map(([k, v]) => `${v} by ${SEAT[k]?.word || k}`)
        .join(', ');
      parts.push(`${plural(flatHits.length, 'fact', 'facts')} (${seatWords}${linkKeyWins ? `; ${linkKeyWins} won on the link key` : ''})`);
    }
    if (freshHits.length) parts.push(`${freshHits.length} fresh, not yet on a page`);
    if (dueSoon.length) parts.push(`${dueSoon.length} closing soon`);
    if (projectDocs.length) parts.push(`${projectDocs.length} project ${projectDocs.length === 1 ? 'note' : 'notes'}`);
    const searchWord = completed && trace.flat_hits_from_completed ? 'the second search, on the completed sentence' : 'the search';
    steps.push({
      phase: 'the memory answers',
      caption: parts.length ? `${searchWord}: ${parts.join(' · ')}` : 'nothing answered — the walk will rely on the doors',
      dur: Math.max(3.2, 1.6 + hitCards.length * 0.6 + motes.length * 0.3 + (clock ? 0.8 : 0)),
      enter() {
        ripple(new THREE.Vector3(0, 0, 2), PALETTE.phosphor);
        const slots = arcSlots(Math.max(hitCards.length, 1), 7.6);
        hitCards.forEach((card, i) => card.place(slots[i], 1, 1, 0.28));
        const mSlots = arcSlots(Math.max(motes.length, 1), 6.2, 3.6, 5);
        motes.forEach((m, i) => m.place(mSlots[i], 1, 1, 0.35));
        if (clock) clock.place(new THREE.Vector3(6.6, 1.6, 1.5), 0.9, 1, 0.3);
        const dSlots = arcSlots(Math.max(docCards.length, 1), 9.8, -3.2, 2);
        docCards.forEach((card, i) => card.place(dSlots[i], 0.8, 0.9, 0.15));
        camGoal(new THREE.Vector3(0, 0.9, 12.8), new THREE.Vector3(0, 0.3, -2));
      },
      update(t) {
        hitCards.forEach((card, i) => {
          const local = clamp01(t * hitCards.length - i);
          const w = card.highlights[0]?.score || 0.4;
          card.target.halo = 0.1 + 0.28 * Math.sin(Math.min(local, 1) * Math.PI) + 0.14 * w;
        });
        if (clock) {
          clock.hand = t;
          if ((this.lastClock || 0) + 0.04 < t) {
            this.lastClock = t;
            clock.draw();
          }
        }
      },
    });
  }

  // Phase 3 — handed over whole.
  if (servedCards.length) {
    const names = servedPages.map((s) => `${s.wiki_id}${s.role === 'speaker' ? ' (speaking)' : ''}`).join(', ');
    steps.push({
      phase: 'handed over whole',
      caption: `${plural(servedCards.length, 'identity card', 'identity cards')} served to the consumer — ${names} — their doors are barred to the walk`,
      dur: Math.max(3, 1.6 + servedCards.length * 0.6),
      enter() {
        const slots = arcSlots(servedCards.length, 4.6, 0.4, 7.5);
        servedCards.forEach((card, i) => card.place(slots[i], 1.1, 1, 0.3));
        for (const card of hitCards) {
          if (!servedCards.includes(card)) card.place(card.target.pos.clone().add(new THREE.Vector3(0, 0, -4)), 0.7, 0.55, 0.06);
        }
        camGoal(new THREE.Vector3(0, 0.8, 13.5), new THREE.Vector3(0, 0.2, 2));
      },
      update(t) {
        if (t > 0.55) {
          servedCards.forEach((card, i) => {
            card.place(HANDED.clone().add(new THREE.Vector3(i * 0.3, -i * 0.45, i * 0.15)), 0.45, 0.9, 0.08);
          });
        }
      },
    });
  }

  // Phase 4 — the doors.
  if (doorCards.length) {
    const counts = {};
    for (const ep of entryPoints) counts[ep.origin] = (counts[ep.origin] || 0) + 1;
    const rails = Object.entries(counts)
      .map(([k, v]) => `${v} by ${FAMILY_WORDS[k] || k}`)
      .join(' · ');
    steps.push({
      phase: 'the doors',
      caption: `${plural(entryPoints.length, 'door', 'doors')} the walk may start from — ${rails} — seeds: ${trace.seed_mode || 'n/a'}`,
      dur: Math.max(3.4, 1.6 + doorCards.length * 0.5),
      enter() {
        const byOrigin = {};
        for (const ep of entryPoints) (byOrigin[ep.origin] ||= []).push(ep);
        for (const [origin, eps] of Object.entries(byOrigin)) {
          eps.forEach((ep, i) => {
            const card = cards.get(keyOf(ep.wiki_id, ep.page));
            if (card) card.place(railSlot(origin, ep.weight, i, eps.length), 0.78, 1, 0.1 + 0.25 * (ep.weight || 0.3));
          });
        }
        for (const card of hitCards) if (!doorCards.includes(card)) card.place(card.target.pos.clone().add(new THREE.Vector3(0, -1, -5)), 0.5, 0.35, 0.03);
        for (const m of motes) m.place(m.target.pos.clone().add(new THREE.Vector3(0, 1.5, -3)), 0.7, 0.5, 0.1);
        if (clock) clock.place(new THREE.Vector3(7.8, 3.6, -3), 0.6, 0.6, 0.1);
        for (const card of docCards) if (!doorCards.includes(card)) card.place(new THREE.Vector3(7.8, -3.6, -2), 0.55, 0.5, 0.05);
        camGoal(new THREE.Vector3(0, 0.4, 15.4), new THREE.Vector3(0, 0, 0));
        this.beamed = false;
      },
      update(t) {
        if (!this.beamed && t > 0.35) {
          this.beamed = true;
          // The fact that opened each door lights the way to it.
          for (const card of doorCards) {
            const hit = card.matchedFact && hitById.get(card.matchedFact);
            if (hit) {
              const from = card.group.position.clone().add(new THREE.Vector3(0, -CARD_H * 0.35, 0.1));
              const to = card.group.position.clone().add(new THREE.Vector3(CARD_W * 0.32, CARD_H * 0.32, 0.1));
              beam(from, to, FAMILY.rag, 1.4);
              ripple(to, FAMILY.rag);
            }
          }
        }
      },
    });
  }

  // Phase 5 — the walk (or its absence).
  const navRan = !isLight && trace.nav_stop != null && hops.length > 0;
  if (isLight || (!navRan && !isNavigate && trace.nav_stop == null)) {
    steps.push({
      phase: isLight ? 'the navigator sleeps' : 'no walk',
      caption: SKIP_SENTENCE,
      dur: 2.6,
      enter() {
        orb.group.visible = true;
        orb.sleep(true);
        orb.group.position.set(0, -2.5, 9);
        orb.target.set(0, -0.4, 7);
        camGoal(new THREE.Vector3(0, 0.6, 13.2), new THREE.Vector3(0, 0, 1));
      },
      update() {},
    });
  }
  if (navRan) {
    steps.push({
      phase: 'the navigator wakes',
      caption: `it will be shown a pool of cards each step and choose what to open — ${plural(charBudget, 'character', 'characters')} of prose to spend`,
      dur: 2.4,
      enter() {
        orb.group.visible = true;
        orb.sleep(false);
        orb.group.position.set(0, -2.5, 9);
        orb.target.set(0, 0.4, 6.5);
        showBudget(0);
        camGoal(new THREE.Vector3(0, 1.2, 12.8), new THREE.Vector3(0, 0.2, 0));
      },
      update() {},
    });
  }

  let collectedSoFar = 0;
  const openedInOrder = [];
  for (const h of hops) for (const o of h.opened || []) openedInOrder.push(o);
  const lastOpened = openedInOrder[openedInOrder.length - 1];

  hops.forEach((hop, hi) => {
    if (!navRan) return;
    const candidates = hop.candidates || [];
    const opened = hop.opened || [];
    const requested = hop.requested || [];
    const refused = requested.filter((r) => !r.opened);
    const byOrigin = {};
    for (const c of candidates) byOrigin[c.origin] = (byOrigin[c.origin] || 0) + 1;
    const poolWords = Object.entries(byOrigin)
      .map(([k, v]) => `${v} by ${FAMILY_WORDS[k] || k}`)
      .join(', ');

    // 5a — the pool.
    steps.push({
      phase: `step ${hi + 1} — the pool`,
      caption: `${plural(candidates.length, 'card', 'cards')} on the table (${poolWords})` + (hop.note ? ` — the navigator: “${hop.note}”` : ''),
      dur: Math.max(2.8, 1.6 + candidates.length * 0.14 + (hop.note ? 1.4 : 0)),
      enter() {
        const centre = new THREE.Vector3(0, 0.4, 5);
        const slots = ringSlots(candidates.length, 5.6, centre, 0.9);
        candidates.forEach((c, i) => {
          const card = ensureCard(c.wiki_id, c.page, FAMILY[c.origin] || PALETTE.dim);
          if (!card.chips.some((x) => x.label.startsWith('door'))) card.addChip(FAMILY_WORDS[c.origin] || c.origin, FAMILY[c.origin] || PALETTE.dim);
          if (c.summary && !card.bodyText) card.bodyText = c.summary;
          if (c.origin === 'link' || c.origin === 'card') card.setAccent(FAMILY[c.origin]);
          card.draw();
          if (card.opacity < 0.05) card.group.position.copy(slots[i]).add(new THREE.Vector3(0, 3, -6));
          card.place(slots[i], 0.62, 0.85, 0.07);
        });
        // Everything not on the table steps back.
        for (const card of cards.values()) {
          if (!candidates.some((c) => keyOf(c.wiki_id, c.page) === keyOf(card.wiki, card.page)) && !card.read && !card.closed) {
            card.place(card.target.pos.clone().add(new THREE.Vector3(0, 0, -6)), 0.4, 0.2, 0.02);
          }
        }
        orb.target.copy(centre);
        camGoal(new THREE.Vector3(0, 1.2, 15.2), new THREE.Vector3(0, 0.6, 0));
      },
      update(t) {
        const sweep = Math.sin(t * Math.PI * 2) * 5.5;
        orb.target.set(sweep * 0.3, 0.4 + Math.sin(t * 6.3) * 0.15, 5);
      },
    });

    // 5b — the choice, and the vetting.
    if (requested.length) {
      const reasons = refused.map((r) => `${r.wiki_id}/${pageOf(r.page)}: ${REFUSAL_WORDS[r.reason] || r.reason || 'refused'}`);
      steps.push({
        phase: `step ${hi + 1} — the choice`,
        caption:
          `it asked for ${plural(requested.length, 'page', 'pages')} — ${opened.length} opened` +
          (refused.length ? `, ${refused.length} refused: ${reasons.join('; ')}` : ''),
        dur: Math.max(2.4, 1.2 + requested.length * 0.6 + refused.length * 0.9),
        enter() {
          let ghosts = 0;
          for (const r of requested) {
            let card = cards.get(keyOf(r.wiki_id, r.page));
            if (!card) {
              // Asked for, never offered: a ghost at the edge of the table.
              card = ensureCard(r.wiki_id, r.page, PALETTE.rose);
              card.group.position.set(7.5, 3.5 - ghosts * 3.2, 0);
              card.place(new THREE.Vector3(7.5, 3.2 - ghosts * 3.2, 1), 0.5, 0.6, 0.1);
              ghosts += 1;
            }
            if (r.opened) {
              beam(orb.group.position, card.group.position, FAMILY.rag, 1.2);
              card.target.halo = 0.35;
            } else {
              card.setAccent(PALETTE.rose);
              card.addChip('refused', PALETTE.rose);
              card.draw();
              card.target.halo = 0.3;
            }
          }
        },
        update(t) {
          if (t > 0.6) {
            for (const r of refused) {
              const card = cards.get(keyOf(r.wiki_id, r.page));
              if (card) card.target.halo = 0.5 * (1 - t);
            }
          }
        },
      });
    }

    // 5c — reading: the prose streams in, the budget drains.
    opened.forEach((o, oi) => {
      const readPose = new THREE.Vector3(3.6, 0.4, 6.2);
      const before = collectedSoFar;
      collectedSoFar += o.chars || 0;
      const after = collectedSoFar;
      const cut = trace.truncated && o === lastOpened;
      steps.push({
        phase: `step ${hi + 1} — reading ${o.wiki_id}/${pageOf(o.page)}`,
        caption:
          `${plural(o.chars || 0, 'character', 'characters')} collected` +
          (o.discovered ? ` — ${plural(o.discovered, 'new door sprouts', 'new doors sprout')} from its links` : '') +
          (cut ? ' — the budget cut it here' : ''),
        dur: Math.max(4.6, 2.6 + Math.min(o.excerpt?.length || 0, 600) * 0.011),
        enter() {
          const card = ensureCard(o.wiki_id, o.page, PALETTE.bright);
          this.card = card;
          card.read = true;
          card.torn = cut;
          card.bodyText = o.excerpt || '';
          card.streamChars = 0;
          card.highlights = card.highlights.slice(0, 1);
          card.draw();
          card.place(readPose, 1.65, 1, 0.25);
          orb.target.copy(readPose).add(new THREE.Vector3(-2.6, 0.3, 1.2));
          beam(orb.group.position, card.group.position, card.accent);
          camGoal(new THREE.Vector3(1.6, 0.7, 11.6), new THREE.Vector3(2.4, 0.2, 4));
          this.parked = false;
        },
        update(t) {
          const card = this.card;
          if (!card) return;
          card.streamChars = Math.floor(easeOut(Math.min(t / 0.85, 1)) * (card.bodyText.length || 0));
          if ((this.lastDraw || 0) + 0.03 < t) {
            this.lastDraw = t;
            card.draw();
          }
          showBudget(lerp(before, after, clamp01(t / 0.85)));
          if (t > 0.9 && !this.parked) {
            this.parked = true;
            // Park the read page on the shelf of the collected, to the left.
            const n = openedInOrder.indexOf(o);
            card.place(SHELF_LEFT.clone().add(new THREE.Vector3(-n * 0.5, n * 0.25, n * 0.6)), 0.5, 0.9, 0.06);
          }
        },
      });
    });
  });

  // Phase 6 — the stop.
  if (navRan) {
    steps.push({
      phase: 'the walk stops',
      caption: STOP_SENTENCES[trace.nav_stop] || trace.nav_stop || '',
      dur: 2.4,
      enter() {
        orb.target.set(0, 0.4, 8);
        camGoal(new THREE.Vector3(0, 1, 13), new THREE.Vector3(0, 0, 0));
      },
      update() {},
    });
  }

  // Phase 7 — the block.
  const injected = trace.injected_block || '';
  const bands = isNavigate ? [] : bandsOf(injected);
  const block = injected && !isNavigate ? new Block(bands, injected.length) : null;
  const dropped = flatHits.filter((h) => h.dropped);
  {
    const kept = flatHits.length - dropped.length;
    // A deep search is nothing but the recall, so one figure is the truth.
    const timing =
      !isNavigate && trace.recall_ms != null && trace.took_ms != null
        ? `recall ${fmt(trace.recall_ms)} ms of a ${fmt(trace.took_ms)} ms turn`
        : trace.took_ms != null
          ? `${fmt(trace.took_ms)} ms`
          : '';
    let caption;
    if (isNavigate) {
      const frags = openedInOrder.length;
      caption = `${plural(frags, 'fragment', 'fragments')} answered to the caller as JSON${injected ? ` — ${fmt(injected.length)} characters` : ''}` + (timing ? ` · ${timing}` : '');
    } else if (injected) {
      const dropWords = dropped.length
        ? ` — ${plural(dropped.length, 'fact fell away', 'facts fell away')} (${[...new Set(dropped.map((d) => DROP_WORDS[d.dropped] || d.dropped))].join('; ')})`
        : '';
      caption = `${fmt(injected.length)} characters handed to the consumer, in ${plural(bands.length, 'band', 'bands')}` +
        (flatHits.length ? ` — ${kept} of ${flatHits.length} facts kept` : '') + dropWords + (timing ? ` · ${timing}` : '');
    } else {
      caption = 'nothing was handed over this turn' + (timing ? ` · ${timing}` : '');
    }
    steps.push({
      phase: isNavigate ? 'the answer' : 'the block',
      caption,
      dur: Math.max(5, 3 + bands.length * 0.5 + dropped.length * 0.4),
      enter() {
        for (const card of cards.values()) {
          if (card.read || card.closed) continue;
          card.place(new THREE.Vector3((Math.random() - 0.5) * 9, -2.6 + Math.random() * 0.6, 7.5 + Math.random()), 0.36, 0.55, 0.05);
        }
        if (isNavigate) {
          // The answer is the pages the walk read: they come forward as the
          // fragments, in the order they were opened.
          const readCards = openedInOrder.map((o) => cards.get(keyOf(o.wiki_id, o.page))).filter(Boolean);
          const slots = arcSlots(Math.max(readCards.length, 1), 6.4, 0.6, 5);
          readCards.forEach((card, i) => card.place(slots[i], 1, 1, 0.22));
        }
        motes.forEach((m, i) => m.place(new THREE.Vector3(-5.2 + i * 1.4, 1.6 - i * 0.5, 8 - i * 0.3), 0.5, 0.6, 0.1));
        if (clock) clock.place(new THREE.Vector3(6.5, -1.6, 7), 0.45, 0.6, 0.06);
        if (block) {
          block.group.position.set(0, -4, 4);
          block.place(new THREE.Vector3(0, 0.1, 5), 1, 1, 0.22);
          block.fill = 0;
          block.draw();
        }
        for (const h of dropped) {
          const card = cardOfHit.get(h.fact_id);
          if (card && !card.read) {
            card.addChip(`dropped: ${DROP_WORDS[h.dropped] || h.dropped}`, PALETTE.rose);
            card.draw();
          }
        }
        orb.target.set(4.6, 2.4, 8.2);
        camGoal(new THREE.Vector3(0, 0.9, 13.4), new THREE.Vector3(0, -0.2, 4));
        this.flown = new Set();
      },
      update(t) {
        if (block) {
          block.fill = easeInOut(clamp01(t / 0.75));
          if ((this.lastDraw || 0) + 0.04 < t) {
            this.lastDraw = t;
            block.draw();
          }
          // The sources fly into their bands as each band lights.
          const sources = [
            ['speaker', servedCards.filter((c) => c.chips.some((x) => x.label === 'speaker'))],
            ['about', servedCards.filter((c) => c.chips.some((x) => x.label === 'named this turn'))],
            ['facts', hitCards.filter((c) => !dropped.some((d) => cardOfHit.get(d.fact_id) === c))],
            ['fresh', motes],
            ['docs', docCards],
            ['walk', [...cards.values()].filter((c) => c.read)],
            ['clock', clock ? [clock] : []],
          ];
          const order = bands.map((b) => b.key);
          for (const [key, list] of sources) {
            const idx = order.indexOf(key);
            if (idx < 0 || this.flown.has(key)) continue;
            if (block.fill * bands.length >= idx + 0.5) {
              this.flown.add(key);
              const y = block.bandCentre(key);
              list.forEach((a, i) => {
                a.place(new THREE.Vector3(2.2 + (i % 3) * 0.3, y + 0.1 - Math.floor(i / 3) * 0.2, 5.4 + i * 0.05), 0.22, 0.8, 0.04);
                beam(a.group.position, new THREE.Vector3(0, y, 5.2), bands[idx].color, 1.0);
              });
            }
          }
          // The dropped ones fall away, to the right, with their reason.
          if (t > 0.5) {
            for (const h of dropped) {
              const card = cardOfHit.get(h.fact_id);
              if (card && !card.read) card.place(new THREE.Vector3(8.2 + Math.random() * 0.4, -3.4 - (t - 0.5) * 2, 6), 0.42, 0.7 * (1.3 - t), 0.03);
            }
          }
        }
      },
    });
  }

  // ---------- timeline engine ----------

  const reduceMotion = window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  let stepIndex = -1;
  let stepT = 0;
  let playing = !reduceMotion;
  let speed = 1;
  let current = null;

  function enterStep(i) {
    stepIndex = i;
    stepT = 0;
    current = steps[i] || null;
    if (!current) return;
    capPhase.textContent = current.phase || '';
    if (current.caption != null) capText.textContent = current.caption;
    current.enter?.();
  }

  function advance(dt) {
    if (!current) return;
    stepT += dt / current.dur;
    if (stepT >= 1) {
      current.update?.(1);
      if (stepIndex + 1 < steps.length) enterStep(stepIndex + 1);
      else {
        playing = false;
        playBtn.textContent = '⟲';
      }
      return;
    }
    current.update?.(clamp01(stepT));
  }

  function resetStage() {
    for (const a of actors()) a.hide();
    for (const card of cards.values()) {
      card.read = false;
      card.torn = false;
      card.streamChars = -1;
      card.bodyText = '';
      card.chips = card.chips.filter((c) => c.label !== 'refused' && !c.label.startsWith('dropped'));
      card.draw();
    }
    orb.group.visible = false;
    orb.sleep(false);
    collectedSoFar = 0;
    budgetBox.hidden = true;
  }

  hud.addEventListener('click', (e) => {
    const btn = e.target.closest('button');
    if (!btn) return;
    const act = btn.dataset.act;
    if (act === 'play') {
      if (stepIndex >= steps.length - 1 && stepT >= 1) {
        resetStage();
        enterStep(0);
      }
      playing = !playing;
      playBtn.textContent = playing ? '⏸' : '▶';
    } else if (act === 'restart') {
      resetStage();
      enterStep(0);
      playing = true;
      playBtn.textContent = '⏸';
    } else if (act === 'fwd' && stepIndex + 1 < steps.length) {
      enterStep(stepIndex + 1);
    } else if (act === 'back' && stepIndex > 0) {
      enterStep(stepIndex - 1);
    }
  });
  hud.querySelector('[data-act="speed"]').addEventListener('change', (e) => {
    speed = parseFloat(e.target.value) || 1;
  });

  // Click a card → read it in the caption (inspect).
  const raycaster = new THREE.Raycaster();
  renderer.domElement.addEventListener('click', (e) => {
    if (Math.hypot(e.clientX - downX, e.clientY - downY) > 6) return;
    const rect = renderer.domElement.getBoundingClientRect();
    const p = new THREE.Vector2(
      ((e.clientX - rect.left) / rect.width) * 2 - 1,
      -((e.clientY - rect.top) / rect.height) * 2 + 1,
    );
    raycaster.setFromCamera(p, camera);
    const hit = raycaster
      .intersectObjects([...cards.values()].map((c) => c.mesh))
      .find((h) => h.object.userData.card);
    if (hit) {
      const card = hit.object.userData.card;
      capPhase.textContent = `${card.wiki}/${card.page}`;
      capText.textContent = card.bodyText || card.highlights.map((h) => h.text).join(' — ') || card.chips.map((c) => c.label).join(' · ');
      card.target.halo = Math.min(card.target.halo + 0.2, 0.5);
    }
  });

  // ---------- render loop ----------

  await (document.fonts?.ready || Promise.resolve());
  for (const card of cards.values()) card.draw();
  resize();
  enterStep(0);
  if (reduceMotion) {
    playing = false;
    playBtn.textContent = '▶';
    capText.textContent = 'reduced motion: press play to replay the recall (or read the trace below)';
  }

  let last = performance.now();
  let trailClock = 0;
  function frame(now) {
    const dt = Math.min((now - last) / 1000, 0.1);
    last = now;
    if (playing) advance(dt * speed);
    progressFill.style.width = `${((stepIndex + clamp01(stepT)) / Math.max(steps.length, 1)) * 100}%`;

    field.rotation.y += dt * 0.004;

    if (orb.group.visible) {
      orb.group.position.lerp(orb.target, clamp01(dt * 6));
      orb.breathe += dt * (orb.asleep ? 1.2 : 3);
      orb.halo.material.opacity = (orb.asleep ? 0.22 : 0.4) + Math.sin(orb.breathe) * 0.12;
      trailClock += dt;
      if (trailClock > 0.05 && playing && !orb.asleep) {
        trailClock = 0;
        spawnTrailDot();
      }
    }
    for (let i = trail.length - 1; i >= 0; i--) {
      trail[i].material.opacity -= dt * 0.8;
      trail[i].scale.multiplyScalar(1 - dt * 0.6);
      if (trail[i].material.opacity <= 0.02) {
        scene.remove(trail[i]);
        trail[i].material.dispose();
        trail.splice(i, 1);
      }
    }
    for (let i = beams.length - 1; i >= 0; i--) {
      beams[i].material.opacity -= dt * beams[i].userData.decay;
      if (beams[i].material.opacity <= 0.02) {
        scene.remove(beams[i]);
        beams[i].geometry.dispose();
        beams[i].material.dispose();
        beams.splice(i, 1);
      }
    }
    for (let i = ripples.length - 1; i >= 0; i--) {
      const r = ripples[i];
      r.scale.multiplyScalar(1 + dt * 2.4);
      r.material.opacity -= dt * 0.5;
      if (r.material.opacity <= 0.02) {
        scene.remove(r);
        r.geometry.dispose();
        r.material.dispose();
        ripples.splice(i, 1);
      }
    }

    for (const a of actors()) a.tween(dt);

    rig.orbit.yaw *= 1 - dt * 0.6;
    rig.orbit.pitch *= 1 - dt * 0.6;
    rig.pos.lerp(rig.posGoal, clamp01(dt * 1.8));
    rig.look.lerp(rig.lookGoal, clamp01(dt * 1.8));
    const off = rig.pos.clone().sub(rig.look);
    const sph = new THREE.Spherical().setFromVector3(off);
    sph.theta += rig.orbit.yaw;
    sph.phi = Math.max(0.4, Math.min(Math.PI - 0.4, sph.phi + rig.orbit.pitch));
    camera.position.copy(rig.look).add(new THREE.Vector3().setFromSpherical(sph));
    camera.lookAt(rig.look);

    renderer.render(scene, camera);
    requestAnimationFrame(frame);
  }
  requestAnimationFrame(frame);
})();
