# WebGL Quorum Topology Visualizer

Interactive 3-D rendering of Stellar's federated quorum trust topology
(issue #288). Parses `stellar-core` quorum-set JSON dumps into a directed
graph and renders it with Three.js/WebGL using a force-directed layout.

## Modules

| Path | Purpose |
| --- | --- |
| `webgl_renderer/quorum_parser.js` | Parses quorum-set dumps (3 shapes) into `{nodes, edges}` with `direct` / `indirect` / `missing` trust classification |
| `webgl_renderer/force_layout.js` | d3-force-3d simulation with manual tick control and a per-frame jank guard |
| `webgl_renderer/grid_repulsion.js` | Allocation-free grid-hashed repulsion + collision force (see Performance) |
| `webgl_renderer/topology_renderer.js` | Three.js scene: instanced node spheres, batched color-coded edge lines, click-to-inspect picking |
| `webgl_renderer/quorum_stream.js` | WebSocket client for live topology updates (snapshot / node / edge / remove) |
| `webgl_renderer/mainnet_snapshot.js` | Deterministic synthetic mainnet-scale snapshot generator (validation + benches) |
| `../components/force_graph.tsx` | React wrapper used by app surfaces |

## Commands

```sh
npm install
npm test          # node:test unit tests (no browser needed)
npm run typecheck # tsc with checkJs over the renderer sources
npm run bench     # 2,000-node performance gate (asserts 60 FPS budget)
npm run dev       # vite demo at demo/ (URL params: ?orgs=100&perOrg=20&ws=wss://…)
npm run build     # production build of the demo
```

## Performance (issue #288 constraint: 2,000 nodes @ 60 FPS)

`d3-force-3d`'s `forceManyBody` rebuilds a Barnes-Hut octree each tick;
CPU profiling at ~2,300 nodes showed octree dispatch alone at ~45 % of
samples (~25 ms/tick) – over the entire 16.7 ms frame budget.
`grid_repulsion.js` replaces it with a uniform spatial-hash grid
(pair count O(n·k), zero per-tick allocation), and the renderer paces
physics adaptively: a frame either runs one tick *or* rewrites GPU
buffers, never both, and skips physics frames proportional to the last
tick's cost.

`npm run bench` output on the reference machine (2,266 nodes /
101,676 edges):

```
frame avg:   ~8 ms over 300 frames  (budget 16.7 ms ⇒ ~118 FPS CPU-side)
frame worst: < 50 ms
heap growth: ~0 MB over 300 frames (no GC spikes)
```

## WebSocket protocol

```jsonc
{ "type": "snapshot", "nodes": [{"id","name","org?"}], "edges": [{"source","target","kind"}] }
{ "type": "node",   "node": {"id","name?","org?"} }     // upsert
{ "type": "edge",   "edge": {"source","target","kind"} } // upsert / reclassify
{ "type": "remove", "id": "G..." }                       // node gone
```

Unknown message types are ignored for forward compatibility.
