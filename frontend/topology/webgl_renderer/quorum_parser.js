/**
 * quorum_parser – turns stellar-core quorum-set JSON dumps into a directed
 * graph structure suitable for the WebGL force-directed renderer.
 *
 * Supported input shapes (all seen in the wild):
 *
 *  1. stellar-core `info` / SCP dump wrapper:
 *     { "quorum_set": { "threshold": 2, "validators": ["G..."],
 *                       "inner_quorum_sets" | "innerQuorumSets": [...] } }
 *  2. Quorum-survey / network-explorer dump:
 *     { "quorum_sets": [ { "threshold": n, "validators": [...],
 *                          "inner_quorum_sets": [...] }, ... ] }
 *  3. Per-validator dump with metadata:
 *     { "validators": [ { "public_key" | "id" | "name": "G...",
 *                         "organization" | "org": "...",
 *                         "quorum_set" | "quorumSet": {...} }, ... ] }
 *
 * Edge semantics:
 *  - `direct`   – target listed at the top level of the source's quorum set
 *  - `indirect` – target reached only through an inner quorum set
 *  - `missing`  – target key referenced but absent from the dump (peer not
 *                 found / out of sync); rendered red by the visualizer
 *
 * @module topology/webgl_renderer/quorum_parser
 */

/** @typedef {'direct'|'indirect'|'missing'} EdgeKind */

/**
 * @typedef {Object} GraphNode
 * @property {string} id        Validator public key (G...)
 * @property {string} name      Human label (falls back to id)
 * @property {string} [org]     Owning organization, when known
 * @property {boolean} missing  True when referenced but not described in dump
 * @property {number} [x]       Layout position (filled by force_layout)
 * @property {number} [y]
 * @property {number} [z]
 */

/**
 * @typedef {Object} GraphEdge
 * @property {string} source  Source node id
 * @property {string} target  Target node id
 * @property {EdgeKind} kind  Trust classification
 * @property {number} [threshold] Quorum threshold of the owning set
 */

/**
 * @typedef {Object} QuorumGraph
 * @property {GraphNode[]} nodes
 * @property {GraphEdge[]} edges
 * @property {number} threshold  Threshold of the root quorum set (if any)
 */

const EDGE_COLORS = Object.freeze({
  direct: 0x2dd4bf,   // teal   – direct trust
  indirect: 0xfbbf24, // amber  – indirect trust (via inner set)
  missing: 0xf43f5e,  // rose   – missing peer
});

/** @param {unknown} v */
function isPlainObject(v) {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

/** @param {unknown} qs @returns {any} */
function normalizeSet(qs) {
  if (!isPlainObject(qs)) return {};
  const s = /** @type {Record<string, any>} */ (qs);
  const inner = s.inner_quorum_sets ?? s.innerQuorumSets ?? s.innerSets ?? [];
  const validators = s.validators ?? s.keys ?? [];
  return {
    threshold: typeof s.threshold === 'number' ? s.threshold : undefined,
    validators: Array.isArray(validators) ? validators : [],
    inner: Array.isArray(inner) ? inner : [],
  };
}

/** @param {unknown} v */
function keyOf(v) {
  if (typeof v === 'string') return v;
  if (isPlainObject(v)) {
    const o = /** @type {Record<string, any>} */ (v);
    const k = o.public_key ?? o.publicKey ?? o.id ?? o.key ?? o.name;
    if (typeof k === 'string') return k;
  }
  return null;
}

/**
 * Walks a (possibly nested) quorum set, emitting trust edges from `sourceId`.
 *
 * @param {string} sourceId
 * @param {unknown} qs
 * @param {QuorumGraph} graph
 * @param {Map<string, GraphNode>} nodeIndex
 * @param {number} depth
 * @param {Set<string>|null} roster Known validator keys from the dump's own
 *   record list, or `null` when the dump carries no roster (quorum-set-only
 *   shapes) – then every referenced key is treated as a real member.
 */
function walkQuorumSet(sourceId, qs, graph, nodeIndex, depth, roster) {
  const set = normalizeSet(qs);

  /** @param {string} target @param {EdgeKind} baseKind @param {number|undefined} threshold */
  const addEdge = (target, baseKind, threshold) => {
    const known = roster === null || roster.has(target) || nodeIndex.has(target);
    const kind = known ? baseKind : 'missing';
    graph.edges.push({ source: sourceId, target, kind, threshold });
    if (known) {
      if (!nodeIndex.has(target)) {
        const member = { id: target, name: target, missing: false };
        nodeIndex.set(target, member);
        graph.nodes.push(member);
      }
    } else if (!nodeIndex.has(target)) {
      const ghost = { id: target, name: target, missing: true };
      nodeIndex.set(target, ghost);
      graph.nodes.push(ghost);
    }
  };

  for (const v of set.validators) {
    const target = keyOf(v);
    if (!target || target === sourceId) continue;
    addEdge(target, 'direct', set.threshold);
  }

  if (depth > 32) return; // guard against pathological / cyclic dumps
  for (const inner of set.inner) {
    const innerSet = normalizeSet(inner);
    for (const v of innerSet.validators) {
      const target = keyOf(v);
      if (!target || target === sourceId) continue;
      addEdge(target, 'indirect', innerSet.threshold ?? set.threshold);
    }
    walkQuorumSet(sourceId, inner, graph, nodeIndex, depth + 1, roster);
  }
}

/**
 * Parses any supported quorum dump into a directed {@link QuorumGraph}.
 *
 * @param {unknown} dump Parsed JSON (object) or a JSON string.
 * @returns {QuorumGraph}
 */
export function parseQuorumDump(dump) {
  const data = typeof dump === 'string' ? JSON.parse(dump) : dump;
  if (!isPlainObject(data) && !Array.isArray(data)) {
    throw new TypeError('quorum dump must be a JSON object or array');
  }

  /** @type {QuorumGraph} */
  const graph = { nodes: [], edges: [], threshold: undefined };
  /** @type {Map<string, GraphNode>} */
  const nodeIndex = new Map();

  /** @param {string} id @param {Partial<GraphNode>} [meta] */
  const ensureNode = (id, meta = {}) => {
    let n = nodeIndex.get(id);
    if (!n) {
      n = { id, name: meta.name ?? id, org: meta.org, missing: meta.missing ?? false };
      nodeIndex.set(id, n);
      graph.nodes.push(n);
    } else {
      if (meta.name && n.name === n.id) n.name = meta.name;
      if (meta.org && !n.org) n.org = meta.org;
      if (meta.missing === false) n.missing = false;
    }
    return n;
  };

  const root = Array.isArray(data) ? null : data;
  const qsRoot =
    root && (root.quorum_set ?? root.quorumSet ?? root.root_quorum_set ?? null);

  /** @type {Set<string>|null} */
  let roster = null; // null → dump has no validator records; all refs are members

  // Shape 3: per-validator metadata list -----------------------------------
  const validators = Array.isArray(data)
    ? data
    : root && Array.isArray(root.validators)
      ? root.validators
      : null;

  if (validators && validators.some(isPlainObject)) {
    roster = new Set();
    for (const v of validators) {
      if (!isPlainObject(v)) continue;
      const id = keyOf(v);
      if (!id) continue;
      roster.add(id);
      ensureNode(id, {
        name: typeof v.name === 'string' ? v.name : id,
        org: typeof (v.organization ?? v.org) === 'string'
          ? (v.organization ?? v.org)
          : undefined,
        missing: false,
      });
    }
    for (const v of validators) {
      if (!isPlainObject(v)) continue;
      const id = keyOf(v);
      if (!id) continue;
      const qs = v.quorum_set ?? v.quorumSet ?? (qsRoot || undefined);
      if (qs) walkQuorumSet(id, qs, graph, nodeIndex, 0, roster);
    }
  } else if (qsRoot) {
    // Shape 1: single wrapped quorum set – needs a source validator.
    const sourceId = typeof (root.self ?? root.node ?? root.validator) === 'string'
      ? (root.self ?? root.node ?? root.validator)
      : '_self_';
    ensureNode(sourceId, { name: sourceId, missing: false });
    graph.threshold = normalizeSet(qsRoot).threshold;
    walkQuorumSet(sourceId, qsRoot, graph, nodeIndex, 0, null);
  } else if (root && Array.isArray(root.quorum_sets ?? root.quorumSets)) {
    // Shape 2: survey-style list of independent quorum sets. Each set is
    // attributed to every member of that set (no single owner).
    const sets = root.quorum_sets ?? root.quorumSets;
    for (const qs of sets) {
      const set = normalizeSet(qs);
      if (graph.threshold === undefined) graph.threshold = set.threshold;
      const members = new Set();
      /** @param {unknown} s */
      const collect = (s, depth = 0) => {
        if (depth > 32) return;
        const ns = normalizeSet(s);
        for (const v of ns.validators) {
          const k = keyOf(v);
          if (k) members.add(k);
        }
        ns.inner.forEach((i) => collect(i, depth + 1));
      };
      collect(qs);
      roster = members;
      for (const m of members) ensureNode(/** @type {string} */ (m), { missing: false });
      for (const m of members) walkQuorumSet(/** @type {string} */ (m), qs, graph, nodeIndex, 0, roster);
    }
  } else {
    throw new TypeError(
      'unrecognized quorum dump: expected quorum_set, quorum_sets, or validators[]',
    );
  }

  // De-duplicate edges (same source/target/kind) – survey dumps are chatty.
  const seen = new Set();
  graph.edges = graph.edges.filter((e) => {
    const k = `${e.source}>${e.target}:${e.kind}`;
    if (seen.has(k)) return false;
    seen.add(k);
    return true;
  });

  return graph;
}

export { EDGE_COLORS };
