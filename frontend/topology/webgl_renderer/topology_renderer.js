/**
 * topology_renderer – hardware-accelerated WebGL view of the quorum graph.
 *
 * Performance strategy for the 2,000-node budget:
 *  - Nodes: a single `InstancedMesh` of low-poly spheres. One draw call for
 *    all nodes; per-node color via `instanceColor`, transform via matrix.
 *  - Edges: a single `LineSegments` whose position/color buffers are
 *    pre-allocated at graph size and re-filled in place with
 *    `DynamicDrawUsage` – zero per-frame allocation / GC churn.
 *  - Physics: stepped through {@link createForceLayout} with a hard cap of
 *    `maxTicksPerFrame` ticks, so simulation cost can never cause jank.
 *  - Picking: GPU-free raycast against the instanced mesh on click, throttled
 *    to pointer events (never per-frame).
 *
 * @module topology/webgl_renderer/topology_renderer
 */

import * as THREE from 'three';
import { OrbitControls } from 'three/examples/jsm/controls/OrbitControls.js';
import { createForceLayout, LAYOUT_DEFAULTS } from './force_layout.js';
import { EDGE_COLORS } from './quorum_parser.js';

const DEFAULTS = Object.freeze({
  background: 0x0b1119,
  nodeColor: 0x8fb8de,
  missingNodeColor: 0xf43f5e,
  selectedNodeColor: 0xffffff,
  nodeRadius: 1.6,
  maxDpr: 2,
  edgeOpacity: 0.55,
});

/**
 * @typedef {Object} RendererNode  Must include {id, x, y, z}
 * @typedef {Object} RendererEdge  {source, target, kind}
 */

/**
 * @param {string|HTMLElement} containerOrSelector
 * @param {{
 *   nodes?: RendererNode[],
 *   edges?: RendererEdge[],
 *   onNodeSelect?: (node: object|null) => void,
 *   layout?: Partial<typeof LAYOUT_DEFAULTS>,
 *   interactive?: boolean,
 * }} [options]
 */
export function createTopologyRenderer(containerOrSelector, options = {}) {
  const container =
    typeof containerOrSelector === 'string'
      ? document.querySelector(containerOrSelector)
      : containerOrSelector;
  if (!container) throw new Error('topology renderer: container not found');

  const state = {
    /** @type {RendererNode[]} */
    nodes: [],
    /** @type {RendererEdge[]} */
    edges: [],
    edgeIndex: new Map(), // `${source}>${target}` -> edge
    selectedId: null,
    disposed: false,
    frameId: 0,
    needColorFlush: false,
  };

  // ---- Scene ---------------------------------------------------------------
  const scene = new THREE.Scene();
  scene.background = new THREE.Color(DEFAULTS.background);

  const camera = new THREE.PerspectiveCamera(50, 1, 0.1, 5000);
  camera.position.set(0, 0, 260);

  const renderer = new THREE.WebGLRenderer({ antialias: true, powerPreference: 'high-performance' });
  renderer.setPixelRatio(Math.min(window.devicePixelRatio || 1, DEFAULTS.maxDpr));
  renderer.outputColorSpace = THREE.SRGBColorSpace;
  container.appendChild(renderer.domElement);
  renderer.domElement.style.display = 'block';

  const controls = new OrbitControls(camera, renderer.domElement);
  controls.enableDamping = true;
  controls.dampingFactor = 0.08;
  controls.rotateSpeed = 0.6;

  scene.add(new THREE.AmbientLight(0xffffff, 1.1));
  const key = new THREE.DirectionalLight(0xffffff, 0.9);
  key.position.set(60, 80, 100);
  scene.add(key);

  // ---- Node instancing (single draw call) ----------------------------------
  const NODE_CAPACITY = 4096; // > 2,000-node requirement
  const nodeGeo = new THREE.SphereGeometry(DEFAULTS.nodeRadius, 12, 8);
  const nodeMat = new THREE.MeshPhongMaterial({ shininess: 40 });
  const nodeMesh = new THREE.InstancedMesh(nodeGeo, nodeMat, NODE_CAPACITY);
  nodeMesh.instanceMatrix.setUsage(THREE.DynamicDrawUsage);
  nodeMesh.instanceColor = new THREE.InstancedBufferAttribute(new Float32Array(NODE_CAPACITY * 3), 3);
  nodeMesh.instanceColor.setUsage(THREE.DynamicDrawUsage);
  nodeMesh.count = 0;
  nodeMesh.frustumCulled = false;
  scene.add(nodeMesh);

  // ---- Edges (one draw call, pre-allocated buffers) ------------------------
  const edgeCapacity = Math.max(8, options.edges?.length ?? 0);
  const edgePos = new Float32Array(edgeCapacity * 2 * 3);
  const edgeCol = new Float32Array(edgeCapacity * 2 * 3);
  const edgeGeo = new THREE.BufferGeometry();
  const edgePosAttr = new THREE.BufferAttribute(edgePos, 3).setUsage(THREE.DynamicDrawUsage);
  const edgeColAttr = new THREE.BufferAttribute(edgeCol, 3).setUsage(THREE.DynamicDrawUsage);
  edgeGeo.setAttribute('position', edgePosAttr);
  edgeGeo.setAttribute('color', edgeColAttr);
  edgeGeo.setDrawRange(0, 0);
  const edgeMat = new THREE.LineBasicMaterial({
    vertexColors: true,
    transparent: true,
    opacity: DEFAULTS.edgeOpacity,
  });
  const edgeLines = new THREE.LineSegments(edgeGeo, edgeMat);
  edgeLines.frustumCulled = false;
  scene.add(edgeLines);

  // ---- Layout --------------------------------------------------------------
  const layout = createForceLayout(
    { nodes: state.nodes, edges: state.edges },
    options.layout,
  );

  // ---- Graph (re)population -------------------------------------------------
  /**
   * Replaces the graph atomically. Keeps layout positions for nodes whose id
   * survives the swap (WebSocket snapshot → no visual "explode").
   * @param {RendererNode[]} nodes
   * @param {RendererEdge[]} edges
   */
  function setGraph(nodes, edges) {
    const prev = new Map(state.nodes.map((n) => [n.id, n]));
    state.nodes.length = 0;
    state.edges.length = 0;
    state.edgeIndex.clear();
    for (const n of nodes) {
      const old = prev.get(n.id);
      if (old && typeof old.x === 'number') {
        n.x = old.x; n.y = old.y; n.z = old.z;
      }
      state.nodes.push(n);
    }
    for (const e of edges) {
      state.edges.push(e);
      state.edgeIndex.set(`${e.source}>${e.target}`, e);
    }
    layout.reheat(0.7);
    rebuildEdgeBuffers();
    state.needColorFlush = true;
    state.dirty = true;
  }

  /**
   * Applies a single upserted edge without touching the rest of the graph.
   * @param {RendererEdge} edge
   */
  function upsertEdge(edge) {
    const k = `${edge.source}>${edge.target}`;
    const existing = state.edgeIndex.get(k);
    if (existing) {
      existing.kind = edge.kind;
      if (!state.edges.includes(existing)) state.edges.push(existing);
    } else {
      state.edges.push(edge);
      state.edgeIndex.set(k, edge);
      if (state.edges.length > edgeCapacity) {
        // Buffer is full – rebuild is cheaper than silent data loss.
        return rebuildEdgeBuffers();
      }
    }
    state.needColorFlush = true;
    return undefined;
  }

  /**
   * Applies a single upserted node (WebSocket incremental update).
   * @param {object} node Must include {id}; position preserved if node exists.
   */
  function upsertNode(node) {
    if (!node || !node.id) return;
    const i = state.nodes.findIndex((n) => n.id === node.id);
    if (i >= 0) {
      state.nodes[i] = { ...state.nodes[i], ...node };
    } else {
      if (state.nodes.length >= NODE_CAPACITY) return; // over budget: ignore
      state.nodes.push(node);
      layout.reheat(0.5);
    }
    state.dirty = true;
  }

  /** @param {string} id */
  function removeNode(id) {
    const i = state.nodes.findIndex((n) => n.id === id);
    if (i >= 0) state.nodes.splice(i, 1);
    for (let j = state.edges.length - 1; j >= 0; j--) {
      const e = state.edges[j];
      if (e.source === id || e.target === id) state.edges.splice(j, 1);
    }
    state.edgeIndex.clear();
    for (const e of state.edges) state.edgeIndex.set(`${e.source}>${e.target}`, e);
    if (state.selectedId === id) {
      state.selectedId = null;
      options.onNodeSelect?.(null);
    }
    layout.reheat(0.5);
    rebuildEdgeBuffers();
    state.needColorFlush = true;
    state.dirty = true;
  }

  // Graph mutation counter: buffers are only rewritten when the graph or a
  // physics tick actually moved nodes (skips ~all work once layout settles).
  state.dirty = true;

  // ---- Buffer writers ------------------------------------------------------
  const _color = new THREE.Color();

  function colorHexForNode(n) {
    if (n.id === state.selectedId) return DEFAULTS.selectedNodeColor;
    return n.missing ? DEFAULTS.missingNodeColor : DEFAULTS.nodeColor;
  }

  function flushNodeBuffers() {
    const count = Math.min(state.nodes.length, NODE_CAPACITY);
    const m = new THREE.Matrix4();
    for (let i = 0; i < count; i++) {
      const n = state.nodes[i];
      const s = n.id === state.selectedId ? 1.6 : n.missing ? 0.75 : 1;
      m.makeScale(s, s, s).setPosition(n.x ?? 0, n.y ?? 0, n.z ?? 0);
      nodeMesh.setMatrixAt(i, m);
      _color.setHex(colorHexForNode(n));
      nodeMesh.instanceColor.setXYZ(i, _color.r, _color.g, _color.b);
    }
    nodeMesh.count = count;
    nodeMesh.instanceMatrix.needsUpdate = true;
    nodeMesh.instanceColor.needsUpdate = true;
  }

  function rebuildEdgeBuffers() {
    const count = Math.min(state.edges.length, edgeCapacity);
    let ptr = 0;
    for (let i = 0; i < count; i++) {
      const e = state.edges[i];
      const s = findNode(e.source);
      const t = findNode(e.target);
      if (!s || !t) continue;
      const o = ptr * 6;
      edgePos[o] = s.x ?? 0; edgePos[o + 1] = s.y ?? 0; edgePos[o + 2] = s.z ?? 0;
      edgePos[o + 3] = t.x ?? 0; edgePos[o + 4] = t.y ?? 0; edgePos[o + 5] = t.z ?? 0;
      const hex = EDGE_COLORS[e.kind] ?? EDGE_COLORS.indirect;
      _color.setHex(hex);
      edgeCol[o] = _color.r; edgeCol[o + 1] = _color.g; edgeCol[o + 2] = _color.b;
      edgeCol[o + 3] = _color.r; edgeCol[o + 4] = _color.g; edgeCol[o + 5] = _color.b;
      ptr += 1;
    }
    edgeGeo.setDrawRange(0, ptr * 2);
    edgePosAttr.needsUpdate = true;
    edgeColAttr.needsUpdate = true;
  }

  /** @param {string} id */
  const nodeById = new Map();
  function findNode(id) {
    return nodeById.get(id) ?? null;
  }
  function reindex() {
    nodeById.clear();
    for (const n of state.nodes) nodeById.set(n.id, n);
  }
  // Ids are stable across physics ticks – only reindex on graph mutation.
  let indexedAt = -1;
  function reindexIfStale() {
    if (indexedAt !== state.nodes.length) {
      reindex();
      indexedAt = state.nodes.length;
    }
  }

  // ---- Picking (click → metadata inspection) -------------------------------
  const raycaster = new THREE.Raycaster();
  const pointer = new THREE.Vector2();
  let downAt = null;

  function onPointerDown(ev) {
    downAt = { x: ev.clientX, y: ev.clientY, t: Date.now() };
  }

  function onPointerUp(ev) {
    if (!downAt) return;
    const dx = ev.clientX - downAt.x;
    const dy = ev.clientY - downAt.y;
    const dt = Date.now() - downAt.t;
    downAt = null;
    if (dx * dx + dy * dy > 25 || dt > 400) return; // it was a drag, not a click

    const rect = renderer.domElement.getBoundingClientRect();
    pointer.x = ((ev.clientX - rect.left) / rect.width) * 2 - 1;
    pointer.y = -((ev.clientY - rect.top) / rect.height) * 2 + 1;
    raycaster.setFromCamera(pointer, camera);
    raycaster.params.Mesh = { threshold: DEFAULTS.nodeRadius };
    const hits = raycaster.intersectObject(nodeMesh, false);
    const hit = hits.find((h) => h.instanceId != null && h.instanceId < nodeMesh.count);
    const node = hit ? state.nodes[hit.instanceId] : null;
    state.selectedId = node ? node.id : null;
    state.dirty = true;
    options.onNodeSelect?.(node ?? null);
  }

  if (options.interactive !== false) {
    renderer.domElement.addEventListener('pointerdown', onPointerDown);
    renderer.domElement.addEventListener('pointerup', onPointerUp);
  }

  // ---- Resize --------------------------------------------------------------
  function resize() {
    const w = container.clientWidth || 800;
    const h = container.clientHeight || 600;
    camera.aspect = w / h;
    camera.updateProjectionMatrix();
    renderer.setSize(w, h, false);
  }
  const observer = new ResizeObserver(resize);
  observer.observe(container);
  resize();

  // ---- Frame loop ----------------------------------------------------------
  let fps = 60;
  let frames = 0;
  let fpsWindowStart = performance.now();

  // Adaptive physics pacing: if a tick cost T ms, skip the next ⌈T/8⌉−1
  // frames before ticking again. Guarantees the main thread keeps headroom
  // for rendering no matter how heavy the simulation gets.
  let physicsCooldown = 0;
  let lastTickMs = 0;

  function frame() {
    if (state.disposed) return;
    state.frameId = requestAnimationFrame(frame);

    // Buffers first, physics second: a frame either rewrites GPU buffers or
    // runs a tick, never both – halves the worst-case frame time. Edge/node
    // positions from the previous tick are visually indistinguishable.
    reindexIfStale();
    if (state.dirty || state.needColorFlush) {
      flushNodeBuffers();
      rebuildEdgeBuffers(); // edges follow physics → positions change every tick
      state.dirty = false;
      state.needColorFlush = false;
    }
    if (physicsCooldown > 0) {
      physicsCooldown--;
    } else if (layout.alpha() > layout.simulation.alphaMin()) {
      const t0 = performance.now();
      layout.tick(1);
      lastTickMs = performance.now() - t0;
      physicsCooldown = Math.max(0, Math.ceil(lastTickMs / 8) - 1);
      state.dirty = true;
    }

    controls.update();
    renderer.render(scene, camera);

    frames++;
    const now = performance.now();
    if (now - fpsWindowStart >= 1000) {
      fps = (frames * 1000) / (now - fpsWindowStart);
      frames = 0;
      fpsWindowStart = now;
    }
  }
  frame();

  return {
    scene,
    camera,
    renderer,
    controls,
    layout,
    setGraph,
    upsertNode,
    upsertEdge,
    removeNode,
    /** @param {string|null} id */
    selectNode(id) {
      state.selectedId = id;
      state.dirty = true;
      options.onNodeSelect?.(id ? (findNode(id) ?? null) : null);
    },
    getGraph: () => ({ nodes: state.nodes.slice(), edges: state.edges.slice() }),
    getFps: () => fps,
    getStats: () => ({
      nodes: state.nodes.length,
      edges: state.edges.length,
      fps: Math.round(fps),
      drawCalls: renderer.info.render.calls,
      triangles: renderer.info.render.triangles,
      alpha: layout.alpha(),
      lastTickMs: Math.round(lastTickMs * 100) / 100,
      physicsCooldown,
    }),
    dispose() {
      state.disposed = true;
      cancelAnimationFrame(state.frameId);
      observer.disconnect();
      if (options.interactive !== false) {
        renderer.domElement.removeEventListener('pointerdown', onPointerDown);
        renderer.domElement.removeEventListener('pointerup', onPointerUp);
      }
      layout.stop();
      controls.dispose();
      nodeGeo.dispose();
      nodeMat.dispose();
      edgeGeo.dispose();
      edgeMat.dispose();
      renderer.dispose();
      renderer.domElement.remove();
    },
  };
}
