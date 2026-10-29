/**
 * demo entry – boots the WebGL quorum topology visualizer with the synthetic
 * mainnet snapshot, optional WebSocket live stream, and metadata inspection.
 *
 * Run: npm run dev   (vite serves this directory)
 *
 * URL params:
 *   ?orgs=100&perOrg=20   size of the synthetic snapshot
 *   ?ws=wss://...         connect a live quorum stream on load
 */
import { parseQuorumDump } from '../webgl_renderer/quorum_parser.js';
import { buildMainnetSnapshot } from '../webgl_renderer/mainnet_snapshot.js';
import { createTopologyRenderer } from '../webgl_renderer/topology_renderer.js';
import { createQuorumStream } from '../webgl_renderer/quorum_stream.js';

const params = new URLSearchParams(location.search);
const orgs = Number(params.get('orgs') ?? 100);
const perOrg = Number(params.get('perOrg') ?? 20);

// ---- Snapshot load ---------------------------------------------------------
const t0 = performance.now();
const snapshot = buildMainnetSnapshot({ orgs, validatorsPerOrg: perOrg });
const graph = parseQuorumDump(snapshot);
const parseMs = performance.now() - t0;
console.info(
  `[topology] parsed ${graph.nodes.length} nodes / ${graph.edges.length} edges ` +
  `in ${parseMs.toFixed(1)} ms`,
);

// ---- Renderer --------------------------------------------------------------
const $ = (id) => document.getElementById(id);
const meta = $('meta');

function renderMeta(node) {
  if (!node) {
    meta.innerHTML = '<span class="empty">click a sphere to inspect metadata</span>';
    return;
  }
  const rows = [
    ['public key', node.id],
    ['name', node.name ?? '–'],
    ['organization', node.org ?? 'unknown'],
    ['status', node.missing ? 'missing peer' : 'reachable'],
  ];
  meta.innerHTML =
    '<div class="kv">' +
    rows.map(([k, v]) => `<span>${k}</span><span>${v}</span>`).join('') +
    '</div>';
}

const renderer = createTopologyRenderer('#viewport', {
  onNodeSelect: renderMeta,
});
renderer.setGraph(graph.nodes, graph.edges);

// ---- Stats HUD (throttled to 4 Hz) -----------------------------------------
setInterval(() => {
  const s = renderer.getStats();
  $('s-nodes').textContent = String(s.nodes);
  $('s-edges').textContent = String(s.edges);
  $('s-fps').textContent = String(s.fps);
  $('s-draw').textContent = String(s.drawCalls);
  $('s-tick').textContent = `${s.lastTickMs} ms`;
}, 250);

// ---- Optional WebSocket live stream ----------------------------------------
let stream = null;

function connectStream(url) {
  stream?.close();
  stream = createQuorumStream({
    url,
    onSnapshot: ({ nodes, edges }) => renderer.setGraph(nodes, edges),
    onNode: (n) => renderer.upsertNode(n),
    onEdge: (e) => renderer.upsertEdge(e),
    onRemove: (id) => renderer.removeNode(id),
    onStatus: (online) => {
      $('ws-status').textContent = online ? 'connected' : 'offline';
      $('ws-status').style.color = online ? '#2dd4bf' : '#f43f5e';
    },
  });
}

$('ws-connect').addEventListener('click', () => {
  const url = /** @type {HTMLInputElement} */ ($('ws-url')).value.trim();
  if (url) connectStream(url);
});

const wsParam = params.get('ws');
if (wsParam) {
  /** @type {HTMLInputElement} */ ($('ws-url')).value = wsParam;
  connectStream(wsParam);
}
