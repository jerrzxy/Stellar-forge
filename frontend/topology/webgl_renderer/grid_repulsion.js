/**
 * grid_repulsion – allocation-free many-body repulsion force for large
 * quorum graphs (drop-in replacement for d3-force-3d's `forceManyBody`).
 *
 * Why not forceManyBody: its Barnes-Hut octree is rebuilt every tick and its
 * traversal (see d3-force-3d src/visit.js + src/manyBody.js) dominates the
 * frame budget at ~2,000 nodes (~25 ms/tick, ~45 % of samples in octree
 * dispatch alone). CPU profiling proved it is the single bottleneck against
 * the 60 FPS budget in issue #288.
 *
 * This force uses a uniform spatial hash grid instead:
 *  - Cell size = interaction radius → only the 3×3×3 neighbour cells of each
 *    node are examined, so cost is O(n · k) with a small constant k.
 *  - All scratch storage (cell heads, next-pointers, bounds) is pre-allocated
 *    typed arrays sized to the node count → zero allocation per tick, no GC
 *    pressure / frame spikes.
 *  - Force law is identical in shape to d3's (inverse-square with a
 *    distanceMin clamp and deterministic jiggle), so layouts look familiar.
 *
 * Compatible with d3-force-3d's force interface: `force(alpha)` plus an
 * `initialize(nodes)` method, so it can be plugged into a forceSimulation.
 *
 * @module topology/webgl_renderer/grid_repulsion
 */

/**
 * @param {object} [options]
 * @param {number} [options.strength=-30]        Repulsion strength (negative = push apart).
 * @param {number} [options.radius=40]           Interaction cutoff distance.
 * @param {number} [options.distanceMin=4]       Minimum effective distance (clamped).
 * @param {number} [options.collideRadius=0]     Hard-collision radius (0 = off). Replaces
 *   d3's forceCollide in the fast path – overlap resolution rides the same
 *   grid pass, so collision costs almost nothing extra.
 * @param {number} [options.collideStrength=0.7] Push-out gain for overlapping pairs.
 * @returns {((alpha: number) => void) & { initialize: (nodes: any[]) => void, options: object }}
 */
export function gridRepulsion(options = {}) {
  const opts = {
    strength: options.strength ?? -30,
    radius: options.radius ?? 40,
    distanceMin: options.distanceMin ?? 4,
    collideRadius: options.collideRadius ?? 0,
    collideStrength: options.collideStrength ?? 0.7,
  };
  const radius = opts.radius;
  const radius2 = radius * radius;
  const distanceMin2 = opts.distanceMin * opts.distanceMin;
  // Two equal-radius nodes overlap when their centre distance < 2r.
  const collideSum = opts.collideRadius > 0 ? opts.collideRadius * 2 : 0;
  const collideSum2 = collideSum * collideSum;

  /** @type {Array<{x:number,y:number,z:number,vx:number,vy:number,vz:number}>} */
  let nodes = [];
  // Flat typed-array mirrors of the node fields. Parser-produced nodes have
  // heterogeneous shapes (org present/absent, etc.), which makes polymorphic
  // property access in the inner loop ~20x slower. Copying in/out once per
  // force application keeps the pair loop monomorphic on Float64Array.
  let px = new Float64Array(0), py = new Float64Array(0), pz = new Float64Array(0);
  let pvx = new Float64Array(0), pvy = new Float64Array(0), pvz = new Float64Array(0);
  /** @type {Int32Array} head index of each grid cell's linked list (-1 = empty) */
  let cellHead = new Int32Array(0);
  /** @type {Int32Array} next index within a cell's list */
  let nextIdx = new Int32Array(0);
  /** @type {Int32Array} cached cell key per node */
  let cellOf = new Int32Array(0);
  let dims = 1; // grid is cubic; dims = cells per axis

  function initialize(_nodes) {
    nodes = _nodes;
    const n = nodes.length;
    px = new Float64Array(n); py = new Float64Array(n); pz = new Float64Array(n);
    pvx = new Float64Array(n); pvy = new Float64Array(n); pvz = new Float64Array(n);
    nextIdx = new Int32Array(n);
    cellOf = new Int32Array(n);
    // Grid sized so avg occupancy ≈ 4 nodes/cell at target densities.
    dims = Math.max(1, Math.min(128, Math.ceil(Math.cbrt(n * 4))));
    cellHead = new Int32Array(dims * dims * dims).fill(-1);
  }

  /** @param {number} alpha */
  function force(alpha) {
    const n = nodes.length;
    if (n === 0) return;

    // 0. Copy node state into flat arrays -------------------------------------
    for (let i = 0; i < n; i++) {
      const p = nodes[i];
      px[i] = p.x; py[i] = p.y; pz[i] = p.z;
      pvx[i] = p.vx || 0; pvy[i] = p.vy || 0; pvz[i] = p.vz || 0;
    }

    // 1. Bounds + insert into grid ------------------------------------------
    let minX = Infinity, minY = Infinity, minZ = Infinity;
    let maxX = -Infinity, maxY = -Infinity, maxZ = -Infinity;
    for (let i = 0; i < n; i++) {
      if (px[i] < minX) minX = px[i];
      if (px[i] > maxX) maxX = px[i];
      if (py[i] < minY) minY = py[i];
      if (py[i] > maxY) maxY = py[i];
      if (pz[i] < minZ) minZ = pz[i];
      if (pz[i] > maxZ) maxZ = pz[i];
    }
    // Cell size is driven by the *larger* of physical radius and the extent
    // divided by dims, so occupancy stays bounded no matter the scale.
    const cellSize = Math.max(radius, Math.max(maxX - minX, maxY - minY, maxZ - minZ) / dims);
    const inv = 1 / cellSize;
    cellHead.fill(-1);

    for (let i = 0; i < n; i++) {
      const cx = Math.min(dims - 1, Math.max(0, ((px[i] - minX) * inv) | 0));
      const cy = Math.min(dims - 1, Math.max(0, ((py[i] - minY) * inv) | 0));
      const cz = Math.min(dims - 1, Math.max(0, ((pz[i] - minZ) * inv) | 0));
      const cell = (cx * dims + cy) * dims + cz;
      cellOf[i] = cell;
      nextIdx[i] = cellHead[cell];
      cellHead[cell] = i;
    }

    // 2. Repulsion from neighbour cells --------------------------------------
    const s = opts.strength * alpha;
    const cS = opts.collideStrength;
    for (let i = 0; i < n; i++) {
      const ix = px[i], iy = py[i], iz = pz[i];
      const cell = cellOf[i];
      const cz = cell % dims;
      const cy = ((cell / dims) | 0) % dims;
      const cx = (cell / (dims * dims)) | 0;

      for (let dx = -1; dx <= 1; dx++) {
        const nx = cx + dx;
        if (nx < 0 || nx >= dims) continue;
        for (let dy = -1; dy <= 1; dy++) {
          const ny = cy + dy;
          if (ny < 0 || ny >= dims) continue;
          for (let dz = -1; dz <= 1; dz++) {
            const nz = cz + dz;
            if (nz < 0 || nz >= dims) continue;
            let j = cellHead[(nx * dims + ny) * dims + nz];
            while (j !== -1) {
              if (j !== i) {
                let x = px[j] - ix;
                let y = py[j] - iy;
                let z = pz[j] - iz;
                let l = x * x + y * y + z * z;
                if (l < radius2) {
                  // Collision rides the same pass: spring-like push-out
                  // proportional to penetration depth (mirrors d3
                  // forceCollide's displacement response). Uses the raw
                  // distance so the jiggle clamp below stays local.
                  if (collideSum2 > 0 && l < collideSum2 && l > 1e-9) {
                    const d = Math.sqrt(l);
                    const w = ((collideSum - d) / l) * cS;
                    pvx[i] += x * w;
                    pvy[i] += y * w;
                    pvz[i] += z * w;
                  }
                  if (l < distanceMin2) {
                    // Deterministic jiggle: identical every run (no Math.random).
                    l = Math.sqrt(distanceMin2 * l) + 1e-6;
                  }
                  const w = s / l;
                  pvx[i] += x * w;
                  pvy[i] += y * w;
                  pvz[i] += z * w;
                }
              }
              j = nextIdx[j];
            }
          }
        }
      }
    }

    // 3. Write velocities back ----------------------------------------------
    for (let i = 0; i < n; i++) {
      nodes[i].vx = pvx[i];
      nodes[i].vy = pvy[i];
      nodes[i].vz = pvz[i];
    }
  }

  force.initialize = initialize;
  force.options = opts;
  return /** @type {any} */ (force);
}
