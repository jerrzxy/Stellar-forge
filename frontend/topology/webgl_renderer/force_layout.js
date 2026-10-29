/**
 * force_layout – d3-force-3d wrapper that gives quorum-graph nodes physical
 * spacing in 3-D space.
 *
 * Design notes for the 2,000-node budget:
 *  - The simulation is stepped manually (`tick(n)`) instead of via d3's timer
 *    so the render loop owns frame pacing and can cap physics work per frame.
 *  - Positions live in the shared node objects, so the renderer reads them
 *    with zero allocation (no per-frame Vector3 churn / GC pressure).
 *
 * @module topology/webgl_renderer/force_layout
 */

import {
  forceSimulation,
  forceLink,
  forceManyBody,
  forceCenter,
  forceCollide,
} from 'd3-force-3d';
// forceCollide is only used on the `preciseCharge` slow path; the default
// fast path folds collision into the grid pass (see grid_repulsion.js).
import { gridRepulsion } from './grid_repulsion.js';

/** Default physics – tuned for ~2,000 interconnected nodes. */
export const LAYOUT_DEFAULTS = Object.freeze({
  linkDistance: 12,
  chargeStrength: -35,
  /** Interaction radius for the grid repulsion – pair count scales with r³. */
  chargeRadius: 14,
  centerStrength: 0.06,
  collideRadius: 3,
  /** Use the octree forceManyBody instead of the fast grid approximation. */
  preciseCharge: false,
  alphaDecay: 0.028,
  velocityDecay: 0.35,
  /** Max physics ticks executed per rendered frame (jank guard). */
  maxTicksPerFrame: 3,
});

/**
 * Creates a stepped 3-D force simulation over an existing graph.
 *
 * Mutates `graph.nodes` in place (x/y/z seeded deterministically when absent)
 * and links by `source`/`target` id strings.
 *
 * @param {{nodes: Array<object>, edges: Array<{source: string, target: string}>}} graph
 * @param {Partial<typeof LAYOUT_DEFAULTS>} [options]
 * @returns {{
 *   tick: (n?: number) => boolean,
 *   reheat: (alpha?: number) => void,
 *   stop: () => void,
 *   alpha: () => number,
 *   simulation: import('d3-force-3d').Simulation<any, any>
 * }}
 */
export function createForceLayout(graph, options = {}) {
  const opts = { ...LAYOUT_DEFAULTS, ...options };
  const { nodes, edges } = graph;

  // Deterministic seeding: uniform sphere scaled by graph size so snapshots
  // render identically between reloads (helps validation + screenshot diffing).
  const R = Math.cbrt(Math.max(nodes.length, 1)) * 12;
  nodes.forEach((n, i) => {
    if (typeof n.x === 'number' && typeof n.y === 'number' && typeof n.z === 'number') return;
    const golden = 2.399963229728653; // sqrt(5)-based sunflower spiral
    const t = i * golden;
    const ring = Math.sqrt((i + 0.5) / Math.max(nodes.length, 1));
    n.x = Math.cos(t) * ring * R;
    n.y = Math.sin(t) * ring * R;
    n.z = (ring - 0.5) * R;
  });

  const byId = new Map(nodes.map((n) => [n.id, n]));
  const links = [];
  for (const e of edges) {
    const s = byId.get(e.source);
    const t = byId.get(e.target);
    // d3 mutates link endpoints into node refs; keep our graph edges as ids
    // by linking on lightweight copies.
    if (s && t && s !== t) links.push({ source: s, target: t });
  }

  const simulation = forceSimulation(nodes, 3)
    .force('link', forceLink(links).id((d) => d.id).distance(opts.linkDistance).strength(0.35))
    .force(
      'charge',
      opts.preciseCharge
        ? forceManyBody().strength(opts.chargeStrength)
        : gridRepulsion({
            strength: opts.chargeStrength,
            radius: opts.chargeRadius,
            collideRadius: opts.collideRadius,
          }),
    )
    .force('center', forceCenter(0, 0, 0).strength(opts.centerStrength));
  if (opts.preciseCharge) simulation.force('collide', forceCollide(opts.collideRadius));
  simulation
    .alphaDecay(opts.alphaDecay)
    .velocityDecay(opts.velocityDecay)
    .stop(); // we drive ticks manually

  // Reduce per-tick link bookkeeping cost on big graphs.
  const linkForce = simulation.force('link');
  if (linkForce && 'iterations' in linkForce) linkForce.iterations(1);

  return {
    simulation,
    /** Advances the simulation. Returns true while it is still "hot". */
    tick(n = opts.maxTicksPerFrame) {
      const count = Math.max(1, Math.min(n, opts.maxTicksPerFrame));
      for (let i = 0; i < count && simulation.alpha() > simulation.alphaMin(); i++) {
        simulation.tick();
      }
      return simulation.alpha() > simulation.alphaMin();
    },
    reheat(alpha = 0.9) {
      simulation.alpha(Math.max(simulation.alpha(), alpha));
    },
    stop() {
      simulation.stop();
    },
    alpha: () => simulation.alpha(),
  };
}
