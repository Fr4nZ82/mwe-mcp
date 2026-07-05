/* Recall-trace 3D replay — the animated route of one recall run.
 *
 * Mounted on #trace-stage by the admin viewer page
 * (/dashboard/admin/recall-traces/:id). Fetches the trace JSON from the
 * sibling /data endpoint and replays it as a guided scene:
 *
 *   1. the turn text appears;
 *   2. similarity (RAG) lights the matched regions on their pages;
 *   3. the entry-point fan arranges by weight (principal / rag / topic /
 *      situational — the four seed families);
 *   4. the navigator — a phosphor orb with an eye — reads the candidate
 *      cards hop by hop, opens pages (their prose streams in), and every
 *      opened page sprouts its discovered links as new faint cards;
 *   5. the yield: what was injected into the consumer.
 *
 * Everything is data-driven from mwe_core::recall_trace::RecallTrace
 * (snake_case fields). Degrades silently: no WebGL / no data → the stage
 * collapses and the textual trace below remains the whole surface.
 *
 * ES module by design — it imports the vendored Three.js (pinned, MIT,
 * three.LICENSE.txt) relative to its own URL, so everything stays
 * self-hosted (PWA / air-gapped friendly, no CDN).
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
  const pageOf = (p) => p || 'index.md';
  const keyOf = (wiki, page) => `${wiki}//${pageOf(page)}`;

  // ---------- palette (the dashboard's phosphor tokens) ----------

  const css = getComputedStyle(document.documentElement);
  const tok = (name, fallback) => (css.getPropertyValue(name) || '').trim() || fallback;
  const PALETTE = {
    bg: tok('--bg', '#060a09'),
    phosphor: tok('--p', '#33ff99'),
    bright: tok('--p-bright', '#7dffc4'),
    dim: tok('--p-dim', '#1d8f5c'),
    text: tok('--text', '#c8ffe3'),
    textDim: tok('--text-dim', '#6fae91'),
    amber: tok('--amber', '#ffc857'),
    sky: tok('--sky', '#6fd3ff'),
    rose: tok('--rose', '#ff6f91'),
  };
  // Seed-family accents: principal anchors in amber, similarity in bright
  // phosphor, topic in sky, situational in rose, link/sibling discoveries dim.
  const FAMILY = {
    principal: PALETTE.amber,
    rag: PALETTE.bright,
    topic: PALETTE.sky,
    situational: PALETTE.rose,
    link: PALETTE.dim,
    page: PALETTE.dim,
  };

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
  scene.fog = new THREE.Fog(new THREE.Color(PALETTE.bg), 26, 46);
  const camera = new THREE.PerspectiveCamera(50, 16 / 9, 0.1, 100);

  function resize() {
    const w = stage.clientWidth || 800;
    const h = stage.clientHeight || 480;
    renderer.setSize(w, h, false);
    camera.aspect = w / h;
    camera.updateProjectionMatrix();
  }
  window.addEventListener('resize', resize);

  // Starfield — the dark of the memory space.
  {
    const N = 350;
    const pos = new Float32Array(N * 3);
    for (let i = 0; i < N; i++) {
      pos[i * 3] = (Math.random() - 0.5) * 80;
      pos[i * 3 + 1] = (Math.random() - 0.5) * 50;
      pos[i * 3 + 2] = -6 - Math.random() * 38;
    }
    const geo = new THREE.BufferGeometry();
    geo.setAttribute('position', new THREE.BufferAttribute(pos, 3));
    const mat = new THREE.PointsMaterial({
      color: new THREE.Color(PALETTE.dim),
      size: 0.05,
      transparent: true,
      opacity: 0.55,
      depthWrite: false,
    });
    scene.add(new THREE.Points(geo, mat));
  }

  // ---------- tiny helpers ----------

  const clamp01 = (x) => Math.min(1, Math.max(0, x));
  const easeInOut = (t) => (t < 0.5 ? 4 * t * t * t : 1 - Math.pow(-2 * t + 2, 3) / 2);
  const easeOut = (t) => 1 - Math.pow(1 - t, 3);
  const lerp = (a, b, t) => a + (b - a) * t;

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

  // ---------- page cards ----------

  const CARD_W = 2.6;
  const CARD_H = 3.3;
  const TEX_W = 416;
  const TEX_H = 528;
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

  /** One floating wiki page. Redraws its own canvas texture on demand. */
  class Card {
    constructor({ wiki, page, accent, weight }) {
      this.wiki = wiki;
      this.page = pageOf(page);
      this.accent = accent || PALETTE.dim;
      this.weight = weight ?? 0.5;
      this.originLabels = [];
      this.bodyText = '';
      this.highlights = []; // [{text, score}] — the glowing matched regions
      this.streamChars = -1; // -1 = no streaming: draw full body

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

      // Kept deliberately tight and faint — the halo is an accent, the
      // card text must stay readable (maintainer 2026-07-03).
      this.halo = radialSprite(this.accent, 0.05);
      this.halo.scale.set(CARD_W * 1.55, CARD_H * 1.3, 1);
      this.halo.material.opacity = 0;
      this.mesh.add(this.halo);

      this.group = new THREE.Group();
      this.group.add(this.mesh);
      this.group.visible = false;
      // Pose targets the per-frame tween chases.
      this.target = { pos: new THREE.Vector3(), scale: 0.001, opacity: 0, halo: 0 };
      this.group.scale.setScalar(0.001);
      scene.add(this.group);
      this.draw();
    }

    setAccent(color) {
      this.accent = color;
      const spr = radialSprite(color, 0.05);
      this.halo.material.map = spr.material.map;
      this.halo.material.needsUpdate = true;
      spr.material.dispose();
    }

    /** Repaint the card texture (header, chips, body, highlights). */
    draw() {
      const g = this.canvas.getContext('2d');
      const W = TEX_W;
      const H = TEX_H;
      g.clearRect(0, 0, W, H);
      // Panel
      g.fillStyle = 'rgba(6, 14, 11, 0.92)';
      g.strokeStyle = this.accent;
      g.lineWidth = 2;
      g.beginPath();
      g.roundRect(1, 1, W - 2, H - 2, 10);
      g.fill();
      g.stroke();
      // Corner ticks (the dashboard's term-panel look)
      g.strokeStyle = this.accent;
      g.lineWidth = 3;
      for (const [x, y, dx, dy] of [
        [8, 8, 1, 1],
        [W - 8, 8, -1, 1],
        [8, H - 8, 1, -1],
        [W - 8, H - 8, -1, -1],
      ]) {
        g.beginPath();
        g.moveTo(x + dx * 14, y);
        g.lineTo(x, y);
        g.lineTo(x, y + dy * 14);
        g.stroke();
      }
      // Header: wiki id + page
      g.fillStyle = PALETTE.bright;
      g.font = `bold 22px ${MONO}`;
      g.fillText(this.wiki, 20, 40, W - 40);
      g.fillStyle = PALETTE.textDim;
      g.font = `16px ${MONO}`;
      g.fillText(this.page, 20, 64, W - 40);
      // Origin chips
      let cx = 20;
      g.font = `13px ${MONO}`;
      for (const label of this.originLabels.slice(0, 4)) {
        const w = g.measureText(label).width + 14;
        g.strokeStyle = FAMILY[label] || PALETTE.dim;
        g.fillStyle = FAMILY[label] || PALETTE.dim;
        g.lineWidth = 1;
        g.strokeRect(cx, 76, w, 20);
        g.fillText(label, cx + 7, 91);
        cx += w + 8;
      }
      g.strokeStyle = 'rgba(120,180,150,0.35)';
      g.lineWidth = 1;
      g.beginPath();
      g.moveTo(20, 106);
      g.lineTo(W - 20, 106);
      g.stroke();

      // Highlighted (matched) regions — the glow of similarity.
      let y = 132;
      g.font = `15px ${MONO}`;
      for (const h of this.highlights.slice(0, 2)) {
        const lines = wrapMono(g, h.text, W - 56).slice(0, 4);
        for (const line of lines) {
          if (y > H - 26) break;
          g.save();
          g.shadowColor = this.accent;
          g.shadowBlur = 8;
          g.fillStyle = 'rgba(51,255,153,0.10)';
          g.fillRect(16, y - 15, W - 32, 21);
          g.restore();
          g.fillStyle = PALETTE.bright;
          g.fillText(line, 24, y, W - 48);
          y += 22;
        }
        if (h.score != null) {
          g.fillStyle = PALETTE.textDim;
          g.font = `12px ${MONO}`;
          g.fillText(`cosine ${h.score.toFixed(3)}`, 24, y);
          g.font = `15px ${MONO}`;
          y += 24;
        }
      }

      // Body prose (streams during the reading phase).
      if (this.bodyText) {
        const shown =
          this.streamChars >= 0 ? this.bodyText.slice(0, this.streamChars) : this.bodyText;
        g.fillStyle = PALETTE.text;
        g.font = `14px ${MONO}`;
        const lines = wrapMono(g, shown, W - 48);
        for (const line of lines) {
          if (y > H - 22) break;
          g.fillText(line, 24, y, W - 48);
          y += 19;
        }
        if (this.streamChars >= 0 && this.streamChars < this.bodyText.length && y <= H - 22) {
          g.fillStyle = PALETTE.bright;
          g.fillRect(24 + g.measureText(lines[lines.length - 1] || '').width + 3, y - 33, 9, 16);
        }
      }
      this.tex.needsUpdate = true;
    }

    /** Per-frame chase of the pose targets. */
    tween(dt) {
      const k = clamp01(dt * 4.2);
      this.group.position.lerp(this.target.pos, k);
      const s = lerp(this.group.scale.x, this.target.scale, k);
      this.group.scale.setScalar(s);
      this.mesh.material.opacity = lerp(this.mesh.material.opacity, this.target.opacity, k);
      this.halo.material.opacity = lerp(this.halo.material.opacity, this.target.halo, k);
      this.group.visible = this.mesh.material.opacity > 0.01;
      if (this.group.visible) this.group.lookAt(camera.position);
    }
  }

  // ---------- the navigator orb ----------

  function makeOrb() {
    const group = new THREE.Group();
    const body = new THREE.Mesh(
      new THREE.SphereGeometry(0.42, 32, 32),
      new THREE.MeshBasicMaterial({ color: new THREE.Color(PALETTE.phosphor) }),
    );
    // The eye: sclera + iris + pupil, billboarded just in front of the body.
    const eyeCanvas = document.createElement('canvas');
    eyeCanvas.width = eyeCanvas.height = 128;
    const eg = eyeCanvas.getContext('2d');
    eg.fillStyle = '#f4fff9';
    eg.beginPath();
    eg.arc(64, 64, 44, 0, Math.PI * 2);
    eg.fill();
    eg.fillStyle = '#0b3d27';
    eg.beginPath();
    eg.arc(64, 64, 26, 0, Math.PI * 2);
    eg.fill();
    eg.fillStyle = '#02120a';
    eg.beginPath();
    eg.arc(64, 64, 13, 0, Math.PI * 2);
    eg.fill();
    eg.fillStyle = 'rgba(255,255,255,0.85)';
    eg.beginPath();
    eg.arc(53, 52, 6, 0, Math.PI * 2);
    eg.fill();
    const eye = new THREE.Sprite(
      new THREE.SpriteMaterial({ map: new THREE.CanvasTexture(eyeCanvas), depthWrite: false }),
    );
    eye.scale.setScalar(0.5);
    eye.position.set(0, 0.05, 0.4);
    const halo = radialSprite(PALETTE.bright, 0.1);
    halo.scale.setScalar(2.1);
    halo.material.opacity = 0.5;
    group.add(halo, body, eye);
    group.visible = false;
    scene.add(group);
    return { group, target: new THREE.Vector3(), breathe: 0 };
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

  // Beams: transient orb→card light lines.
  const beams = [];
  function beam(from, to, color) {
    const geo = new THREE.BufferGeometry().setFromPoints([from.clone(), to.clone()]);
    const mat = new THREE.LineBasicMaterial({
      color: new THREE.Color(color || PALETTE.bright),
      transparent: true,
      opacity: 0.9,
      blending: THREE.AdditiveBlending,
      depthWrite: false,
    });
    const line = new THREE.Line(geo, mat);
    scene.add(line);
    beams.push(line);
  }

  // ---------- overlay DOM (captions + controls) ----------

  const hud = document.createElement('div');
  hud.className = 'trace-hud';
  hud.innerHTML = `
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

  // ---------- build the cast from the trace ----------

  const cards = new Map(); // key → Card

  function pageFromSourcePath(wiki, sourcePath) {
    // "wikis/famiglia/salute.md" → "salute.md" (best-effort: strip up to
    // and including the wiki id segment).
    const marker = `/${wiki}/`;
    const i = (sourcePath || '').lastIndexOf(marker);
    if (i >= 0) return sourcePath.slice(i + marker.length);
    const parts = (sourcePath || '').split('/');
    return parts[parts.length - 1] || 'index.md';
  }

  function ensureCard(wiki, page, accent, weight) {
    const key = keyOf(wiki, page);
    let card = cards.get(key);
    if (!card) {
      card = new Card({ wiki, page, accent, weight });
      cards.set(key, card);
    }
    return card;
  }

  const flatHits = trace.flat_hits || [];
  const entryPoints = trace.entry_points || [];
  const hops = trace.hops || [];

  // RAG hit cards (phase 2): one per source page, highlights attached.
  const ragCards = [];
  for (const hit of flatHits) {
    const page = pageFromSourcePath(hit.wiki_id, hit.source_path);
    const card = ensureCard(hit.wiki_id, page, FAMILY.rag, hit.score);
    if (card.highlights.length < 2) card.highlights.push({ text: hit.text, score: hit.score });
    if (!card.originLabels.includes('rag')) card.originLabels.push('rag');
    if (!ragCards.includes(card)) ragCards.push(card);
    card.draw();
  }

  // Fan cards (phase 3): every entry point, reusing RAG cards where they
  // coincide. Kept in fan order (already weight-sorted by the gatherer).
  const fanCards = [];
  for (const ep of entryPoints) {
    const card = ensureCard(ep.wiki_id, ep.page, FAMILY[ep.origin] || PALETTE.dim, ep.weight);
    if (!card.originLabels.includes(ep.origin)) card.originLabels.push(ep.origin);
    card.weight = Math.max(card.weight ?? 0, ep.weight ?? 0);
    if (card.originLabels[0] !== 'rag') card.setAccent(FAMILY[card.originLabels[0]] || PALETTE.dim);
    if (!fanCards.includes(card)) fanCards.push(card);
    card.draw();
  }

  // Hop-discovered candidate cards spawn lazily during the replay.

  // Arc layout for the fan: heaviest in the middle, alternating outwards.
  function arcSlots(n, radius = 8.4, y = 0) {
    const slots = [];
    const spread = Math.min(Math.PI * 0.72, 0.32 * Math.max(n - 1, 1));
    for (let i = 0; i < n; i++) {
      // 0, +1, -1, +2, -2 … around the centre.
      const order = Math.ceil(i / 2) * (i % 2 === 1 ? 1 : -1);
      const a = n > 1 ? (order / Math.max(n - 1, 1)) * spread : 0;
      slots.push(
        new THREE.Vector3(Math.sin(a) * radius, y + Math.cos(order) * 0.35, -Math.cos(a) * radius + 4),
      );
    }
    return slots;
  }

  // ---------- camera rig ----------

  const rig = {
    pos: new THREE.Vector3(0, 0.6, 13),
    look: new THREE.Vector3(0, 0, 0),
    posGoal: new THREE.Vector3(0, 0.6, 13),
    lookGoal: new THREE.Vector3(0, 0, 0),
    orbit: { yaw: 0, pitch: 0 }, // small user drag offset, decays back
  };
  function camGoal(pos, look) {
    rig.posGoal.copy(pos);
    rig.lookGoal.copy(look);
  }

  // Light drag-to-peek (does not fight the guided camera — offsets decay).
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

  /** Steps: { phase, caption, dur, enter(), update(t) } — t ∈ [0,1]. */
  const steps = [];
  const fmt = (n) => new Intl.NumberFormat('en-US').format(n);

  const turnText = trace.turn_text || '(empty turn)';
  const sourceLabel = data.source === 'navigate' ? 'deep search (wiki_navigate)' : 'ingest turn';

  // Phase 1 — the turn.
  steps.push({
    phase: `${sourceLabel} · ${data.sender_id}`,
    caption: '',
    dur: Math.min(5, 1.6 + turnText.length * 0.02),
    enter() {
      camGoal(new THREE.Vector3(0, 0.6, 13), new THREE.Vector3(0, 0, 0));
    },
    update(t) {
      const n = Math.floor(easeOut(t) * turnText.length);
      capText.textContent = `“${turnText.slice(0, n)}${n < turnText.length ? '▌' : '”'}`;
    },
  });

  // Phase 2 — similarity lights the doors.
  const fresh = trace.fresh_hits || [];
  steps.push({
    phase: 'similarity (RAG)',
    caption: flatHits.length
      ? `the embedding matched ${flatHits.length} region(s) on ${ragCards.length} page(s)` +
        (fresh.length ? ` · +${fresh.length} fresh (not yet consolidated)` : '')
      : 'no similarity hits — the fan will rely on identity and topic seeds',
    dur: Math.max(2.4, 1.1 + ragCards.length * 0.7),
    enter() {
      const slots = arcSlots(Math.max(ragCards.length, 1), 7.6);
      ragCards.forEach((card, i) => {
        card.target.pos.copy(slots[i]);
        card.target.scale = 1;
        card.target.opacity = 1;
        card.target.halo = 0.28;
      });
      camGoal(new THREE.Vector3(0, 0.8, 12.4), new THREE.Vector3(0, 0, -2));
    },
    update(t) {
      // Staggered glow pulse, score-bright.
      ragCards.forEach((card, i) => {
        const local = clamp01(t * ragCards.length - i);
        card.target.halo = 0.1 + 0.28 * Math.sin(Math.min(local, 1) * Math.PI) + 0.12 * (card.weight || 0);
      });
    },
  });

  // Phase 3 — the fan.
  if (entryPoints.length) {
    const counts = {};
    for (const ep of entryPoints) counts[ep.origin] = (counts[ep.origin] || 0) + 1;
    steps.push({
      phase: 'entry-point fan',
      caption:
        `${entryPoints.length} entrance(s): ` +
        Object.entries(counts)
          .map(([k, v]) => `${v} ${k}`)
          .join(' · ') +
        ` — seeds: ${trace.seed_mode || 'n/a'}`,
      dur: Math.max(2.6, 1.2 + fanCards.length * 0.35),
      enter() {
        const slots = arcSlots(fanCards.length, 8.6);
        fanCards.forEach((card, i) => {
          card.target.pos.copy(slots[i]);
          card.target.scale = 1 - 0.35 * i / Math.max(fanCards.length, 1);
          card.target.scale = Math.max(card.target.scale, 0.62);
          card.target.opacity = 1;
          card.target.halo = 0.08 + 0.22 * (card.weight || 0.3);
        });
        // Cards that were RAG-only and did not make the fan fade back.
        for (const card of ragCards) {
          if (!fanCards.includes(card)) {
            card.target.opacity = 0.25;
            card.target.scale = 0.5;
            card.target.halo = 0.03;
          }
        }
        camGoal(new THREE.Vector3(0, 1.4, 13.6), new THREE.Vector3(0, 0, -3));
      },
      update() {},
    });
  }

  // Phase 4 — the navigator walks.
  const navRan = trace.nav_stop != null;
  if (navRan && hops.length) {
    steps.push({
      phase: 'the navigator wakes',
      caption: 'one funnel, one librarian — it reads the cards, not the prose',
      dur: 2.2,
      enter() {
        orb.group.visible = true;
        orb.group.position.set(0, -2.5, 9);
        orb.target.set(0, 0.4, 6.5);
        camGoal(new THREE.Vector3(0, 1.2, 12.8), new THREE.Vector3(0, 0.2, 0));
      },
      update() {},
    });
  }

  const STOP_CAPTIONS = {
    done: 'the navigator judged the collection sufficient',
    budget: 'the prose budget ran out',
    hop_cap: 'the depth dial ran out',
    llm_degraded: 'the navigator LLM failed — the turn survived on what was collected',
    nothing_opened: 'every pick was vetted away — the funnel stopped',
    pool_exhausted: 'no unvisited candidates remained',
    empty_fan: 'the fan was empty — nothing to walk',
  };

  hops.forEach((hop, hi) => {
    const candidates = hop.candidates || [];
    const opened = hop.opened || [];
    const requested = hop.requested || [];
    const discarded = requested.filter((r) => !r.opened);

    // 4a — look at the candidates.
    steps.push({
      phase: `hop ${hi + 1} — the choice`,
      caption:
        `${candidates.length} candidate(s) on the table` +
        (hop.note ? ` — navigator: “${hop.note}”` : hop.done ? ' — navigator: enough.' : ''),
      dur: Math.max(2.6, 1.5 + candidates.length * 0.12 + (hop.note ? 1.4 : 0)),
      enter() {
        // Candidates not yet on stage (links/siblings discovered earlier)
        // appear as faint outer cards.
        const newcomers = [];
        for (const c of candidates) {
          const key = keyOf(c.wiki_id, c.page);
          if (!cards.has(key)) newcomers.push(c);
        }
        const outer = arcSlots(Math.max(newcomers.length, 1), 11.5, 1.6);
        newcomers.forEach((c, i) => {
          const card = ensureCard(c.wiki_id, c.page, FAMILY[c.origin] || PALETTE.dim, 0.3);
          if (!card.originLabels.includes(c.origin)) card.originLabels.push(c.origin);
          if (c.summary && !card.bodyText) card.bodyText = c.summary;
          card.draw();
          card.group.position.copy(outer[i]).add(new THREE.Vector3(0, 3, -4));
          card.target.pos.copy(outer[i]);
          card.target.scale = 0.55;
          card.target.opacity = 0.65;
          card.target.halo = 0.06;
        });
        orb.target.set(0, 0.6, 5.4);
        camGoal(new THREE.Vector3(0, 1.5, 13.8), new THREE.Vector3(0, 0.3, -2));
      },
      update(t) {
        // The eye sweeps the candidate arc.
        const sweep = Math.sin(t * Math.PI * 2) * 6;
        orb.target.set(sweep, 0.6 + Math.sin(t * 6.3) * 0.15, 5.4);
      },
    });

    // 4b — discarded picks (vetting) flash and dissolve.
    if (discarded.length) {
      steps.push({
        phase: `hop ${hi + 1} — vetting`,
        caption: `${discarded.length} pick(s) discarded — not among the offered candidates (or already read)`,
        dur: 1.8,
        enter() {
          for (const r of discarded) {
            const card = cards.get(keyOf(r.wiki_id, r.page));
            if (card) {
              card.setAccent(PALETTE.rose);
              card.target.halo = 0.25;
              card.draw();
            }
          }
        },
        update(t) {
          if (t > 0.6) {
            for (const r of discarded) {
              const card = cards.get(keyOf(r.wiki_id, r.page));
              if (card) card.target.halo = 0.5 * (1 - t);
            }
          }
        },
      });
    }

    // 4c — open each page: the orb travels, the prose streams in.
    opened.forEach((o, oi) => {
      const readPose = new THREE.Vector3(3.6, 0.4, 6.2);
      steps.push({
        phase: `hop ${hi + 1} — reading ${o.wiki_id}/${o.page}`,
        caption:
          `${fmt(o.chars)} chars collected` +
          (o.discovered ? ` — ${o.discovered} new door(s) sprout from its links` : ''),
        dur: Math.max(5.5, 3 + Math.min(o.excerpt?.length || 0, 700) * 0.013),
        enter() {
          const card = ensureCard(o.wiki_id, o.page, PALETTE.bright, 0.8);
          this.card = card;
          card.bodyText = o.excerpt || '';
          card.streamChars = 0;
          card.highlights = card.highlights.slice(0, 1);
          card.draw();
          this.fromPos = card.group.position.clone();
          card.target.pos.copy(readPose);
          card.target.scale = 1.65;
          card.target.opacity = 1;
          card.target.halo = 0.25;
          orb.target.copy(readPose).add(new THREE.Vector3(-2.6, 0.3, 1.2));
          beam(orb.group.position, card.group.position, card.accent);
          camGoal(new THREE.Vector3(1.6, 0.7, 11.6), new THREE.Vector3(2.4, 0.2, 4));
        },
        update(t) {
          const card = this.card;
          if (!card) return;
          card.streamChars = Math.floor(easeOut(Math.min(t / 0.85, 1)) * (card.bodyText.length || 0));
          if ((this.lastDraw || 0) + 0.03 < t) {
            this.lastDraw = t;
            card.draw();
          }
          if (t > 0.9) {
            // Park the read page to the collected shelf on the left.
            card.target.pos.set(-7.5 - (this.shelfJitter ?? (this.shelfJitter = Math.random())), -1.6, 7 + oi);
            card.target.scale = 0.5;
            card.target.halo = 0.06;
          }
        },
      });
    });
  });

  // Stop reason (only when navigation ran).
  if (navRan) {
    steps.push({
      phase: 'the funnel stops',
      caption: STOP_CAPTIONS[trace.nav_stop] || trace.nav_stop || '',
      dur: 2.2,
      enter() {
        orb.target.set(0, 0.4, 8);
        camGoal(new THREE.Vector3(0, 1, 13), new THREE.Vector3(0, 0, 0));
      },
      update() {},
    });
  }

  // Phase 5 — the yield.
  {
    const injected = trace.injected_block || '';
    const dueN = (trace.due_soon || []).length;
    const parts = [];
    if (flatHits.length) parts.push(`${flatHits.length} flat hit(s)`);
    if (fresh.length) parts.push(`${fresh.length} fresh`);
    const fragTotal = hops.reduce((n, h) => n + (h.opened || []).length, 0);
    if (fragTotal) parts.push(`${fragTotal} navigated page(s)`);
    if (dueN) parts.push(`${dueN} due-soon`);
    steps.push({
      phase: 'the yield',
      caption: injected
        ? `${parts.join(' + ') || 'nothing'} → ${fmt(injected.length)} chars injected into the consumer (full text below)` +
          (trace.took_ms != null ? ` · ${fmt(trace.took_ms)} ms` : '')
        : 'nothing was injected this turn',
      dur: 4.5,
      enter() {
        for (const card of cards.values()) {
          card.target.pos.set(
            (Math.random() - 0.5) * 3,
            -1.8 + Math.random() * 0.6,
            8.5 + Math.random(),
          );
          card.target.scale = 0.42;
          card.target.opacity = 0.85;
          card.target.halo = 0.12;
        }
        orb.target.set(0, 0.2, 8.6);
        camGoal(new THREE.Vector3(0, 0.9, 13.4), new THREE.Vector3(0, -0.4, 4));
      },
      update() {},
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

  hud.addEventListener('click', (e) => {
    const btn = e.target.closest('button');
    if (!btn) return;
    const act = btn.dataset.act;
    if (act === 'play') {
      if (stepIndex >= steps.length - 1 && stepT >= 1) enterStep(0);
      playing = !playing;
      playBtn.textContent = playing ? '⏸' : '▶';
    } else if (act === 'restart') {
      for (const card of cards.values()) {
        card.target.opacity = 0;
        card.target.scale = 0.001;
        card.target.halo = 0;
      }
      orb.group.visible = false;
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

  // Click a card → bring it forward briefly (inspect).
  const raycaster = new THREE.Raycaster();
  renderer.domElement.addEventListener('click', (e) => {
    // A drag release synthesizes a click — inspect only on a still tap.
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
      capText.textContent = card.bodyText || card.highlights.map((h) => h.text).join(' — ') || '';
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

    // Orb motion + trail + breathing halo.
    if (orb.group.visible) {
      orb.group.position.lerp(orb.target, clamp01(dt * 6));
      orb.breathe += dt * 3;
      orb.group.children[0].material.opacity = 0.4 + Math.sin(orb.breathe) * 0.12;
      trailClock += dt;
      if (trailClock > 0.05 && playing) {
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
      beams[i].material.opacity -= dt * 0.7;
      if (beams[i].material.opacity <= 0.02) {
        scene.remove(beams[i]);
        beams[i].geometry.dispose();
        beams[i].material.dispose();
        beams.splice(i, 1);
      }
    }

    for (const card of cards.values()) card.tween(dt);

    // Camera: chase goals, add the (decaying) user-drag offset.
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
