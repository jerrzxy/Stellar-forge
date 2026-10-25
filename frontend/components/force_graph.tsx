/**
 * ForceGraph – React wrapper around the WebGL quorum topology renderer
 * (issue #288).
 *
 * Renders a stellar-core quorum dump as an interactive 3-D force-directed
 * graph. Nodes are instanced spheres, edges are batched lines color-coded by
 * trust kind (direct / indirect / missing). When `wsUrl` is given the graph
 * updates live over WebSocket.
 *
 * The heavy lifting lives in `frontend/topology/webgl_renderer/`; this file
 * only manages the React lifecycle around it:
 *  - create the renderer once on mount
 *  - feed prop changes (parsed dump / snapshot) through `setGraph`
 *  - tear everything down on unmount (no GPU leaks)
 *
 * @module components/force_graph
 */

import { useEffect, useRef } from 'react';
// @ts-expect-error – plain-JS renderer module (checked by topology/tsconfig)
import { createTopologyRenderer } from '../topology/webgl_renderer/topology_renderer.js';
// @ts-expect-error – plain-JS parser module (checked by topology/tsconfig)
import { parseQuorumDump } from '../topology/webgl_renderer/quorum_parser.js';

export interface QuorumNode {
  id: string;
  name?: string;
  org?: string;
  missing?: boolean;
}

export interface QuorumEdge {
  source: string;
  target: string;
  kind: 'direct' | 'indirect' | 'missing';
}

export interface QuorumGraph {
  nodes: QuorumNode[];
  edges: QuorumEdge[];
  threshold?: number;
}

export interface ForceGraphProps {
  /** Parsed quorum graph (from `parseQuorumDump`) or a raw stellar-core dump
   *  object/JSON string; re-parsed via memoized `parseQuorumDump`. */
  data: QuorumGraph | Record<string, unknown> | string | null;
  /** ws:// or wss:// endpoint streaming live quorum updates (optional). */
  wsUrl?: string;
  /** Click handler for node metadata inspection. */
  onNodeSelect?: (node: QuorumNode | null) => void;
  /** CSS width of the canvas container (default: 100%). */
  width?: number | string;
  /** CSS height of the canvas container (default: 480px). */
  height?: number | string;
  /** Accessibility label for the canvas. */
  ariaLabel?: string;
}

export default function ForceGraph({
  data,
  wsUrl,
  onNodeSelect,
  width = '100%',
  height = 480,
  ariaLabel = 'Interactive 3D quorum topology graph',
}: ForceGraphProps) {
  const mountRef = useRef<HTMLDivElement>(null);
  const rendererRef = useRef<ReturnType<typeof createTopologyRenderer> | null>(null);
  const streamRef = useRef<{ close: () => void } | null>(null);
  const onSelectRef = useRef(onNodeSelect);
  onSelectRef.current = onNodeSelect;

  // ---- one-time renderer lifecycle ----------------------------------------
  useEffect(() => {
    const mount = mountRef.current;
    if (!mount) return;

    const renderer = createTopologyRenderer(mount, {
      onNodeSelect: (node) => onSelectRef.current?.(node ?? null),
    });
    rendererRef.current = renderer;

    return () => {
      streamRef.current?.close();
      streamRef.current = null;
      renderer.dispose();
      rendererRef.current = null;
    };
  }, []);

  // ---- data changes (memoized parse) ---------------------------------------
  useEffect(() => {
    const renderer = rendererRef.current;
    if (!renderer || data == null) return;

    let graph: QuorumGraph;
    if (typeof data === 'object' && 'nodes' in data && 'edges' in data) {
      graph = data as QuorumGraph; // already parsed
    } else {
      graph = parseQuorumDump(data) as QuorumGraph;
    }
    renderer.setGraph(graph.nodes, graph.edges);
  }, [data]);

  // ---- live stream ----------------------------------------------------------
  useEffect(() => {
    const renderer = rendererRef.current;
    if (!renderer || !wsUrl) return;

    let cancelled = false;
    import('../topology/webgl_renderer/quorum_stream.js')
      .then(({ createQuorumStream }) => {
        if (cancelled || !rendererRef.current) return;
        streamRef.current = createQuorumStream({
          url: wsUrl,
          onSnapshot: ({ nodes, edges }) => rendererRef.current?.setGraph(nodes, edges),
          onNode: (n) => rendererRef.current?.upsertNode(n),
          onEdge: (e) => rendererRef.current?.upsertEdge(e),
          onRemove: (id) => rendererRef.current?.removeNode(id),
        });
      })
      .catch((err) => console.error('[force_graph] stream setup failed:', err));

    return () => {
      cancelled = true;
      streamRef.current?.close();
      streamRef.current = null;
    };
  }, [wsUrl]);

  return (
    <div
      ref={mountRef}
      role="img"
      aria-label={ariaLabel}
      data-testid="force-graph"
      style={{
        width: typeof width === 'number' ? `${width}px` : width,
        height: typeof height === 'number' ? `${height}px` : height,
        overflow: 'hidden',
        background: '#0b1119',
      }}
    />
  );
}
