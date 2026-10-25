/**
 * quorum_stream – WebSocket client that keeps the quorum graph live.
 *
 * Protocol (JSON messages, one object per frame):
 *
 *   { type: "snapshot", nodes: [{id,name,org?}], edges: [{source,target,kind}],
 *     threshold?: number }
 *   { type: "node",   node: {id, name?, org?} }             // upsert
 *   { type: "edge",   edge: {source, target, kind} }        // upsert / reclassify
 *   { type: "remove", id: "G..." }                          // node gone
 *   { type: "status", node: "G...", online: boolean }       // soft state, no graph change
 *
 * The transport is injectable (`WebSocketImpl`) so tests can drive the
 * protocol without a network. Unknown message types are ignored (forward
 * compatibility with newer stellar-core streams).
 *
 * @module topology/webgl_renderer/quorum_stream
 */

/**
 * @typedef {Object} QuorumStreamOptions
 * @property {string} url                         ws:// or wss:// endpoint
 * @property {(graph: {nodes: Array<object>, edges: Array<object>}) => void} onSnapshot
 * @property {(node: object) => void} [onNode]
 * @property {(edge: object) => void} [onEdge]
 * @property {(id: string) => void} [onRemove]
 * @property {(online: boolean) => void} [onStatus] Socket connectivity callback
 * @property {number} [reconnectMs] Base reconnect delay (default 1000, exp backoff ×2, cap 15s)
 * @property {Function} [WebSocketImpl] Injected WebSocket constructor
 */

/**
 * @param {QuorumStreamOptions} options
 * @returns {{ close: () => void, socket: WebSocket | null, graph: {nodes: Map<string, object>, edges: Map<string, object>} }}
 */
export function createQuorumStream(options) {
  const {
    url,
    onSnapshot,
    onNode,
    onEdge,
    onRemove,
    onStatus,
    reconnectMs = 1000,
    WebSocketImpl = (typeof globalThis !== 'undefined' && globalThis.WebSocket),
  } = options;

  if (typeof WebSocketImpl !== 'function') {
    throw new Error('no WebSocket implementation available; pass WebSocketImpl');
  }

  /** Aggregated graph state – the renderer diffs against this. */
  const graph = { nodes: new Map(), edges: new Map() };
  const edgeKey = (e) => `${e.source}>${e.target}`;

  let socket = null;
  let closed = false;
  let attempt = 0;
  let reconnectTimer = null;

  /** @param {string} raw */
  function handleMessage(raw) {
    let msg;
    try {
      msg = JSON.parse(raw);
    } catch {
      return; // ignore malformed frames
    }
    switch (msg.type) {
      case 'snapshot': {
        graph.nodes.clear();
        graph.edges.clear();
        for (const n of msg.nodes ?? []) if (n && n.id) graph.nodes.set(n.id, n);
        for (const e of msg.edges ?? []) {
          if (e && e.source && e.target) graph.edges.set(edgeKey(e), e);
        }
        onSnapshot?.({ nodes: [...graph.nodes.values()], edges: [...graph.edges.values()] });
        break;
      }
      case 'node': {
        const n = msg.node;
        if (!n || !n.id) return;
        const prev = graph.nodes.get(n.id);
        graph.nodes.set(n.id, prev ? { ...prev, ...n } : n);
        onNode?.(graph.nodes.get(n.id));
        break;
      }
      case 'edge': {
        const e = msg.edge;
        if (!e || !e.source || !e.target) return;
        graph.edges.set(edgeKey(e), e);
        onEdge?.(e);
        break;
      }
      case 'remove': {
        if (typeof msg.id !== 'string') return;
        graph.nodes.delete(msg.id);
        for (const [k, e] of graph.edges) {
          if (e.source === msg.id || e.target === msg.id) graph.edges.delete(k);
        }
        onRemove?.(msg.id);
        break;
      }
      case 'status':
      default:
        // `status` and unknown types don't mutate topology.
        break;
    }
  }

  function connect() {
    if (closed) return;
    socket = new (/** @type {any} */ (WebSocketImpl))(url);
    socket.onopen = () => {
      attempt = 0;
      onStatus?.(true);
    };
    socket.onmessage = (ev) => handleMessage(typeof ev.data === 'string' ? ev.data : '');
    socket.onclose = () => {
      onStatus?.(false);
      if (closed) return;
      const delay = Math.min(reconnectMs * 2 ** attempt, 15000);
      attempt += 1;
      reconnectTimer = setTimeout(connect, delay);
    };
    socket.onerror = () => socket?.close();
  }

  connect();

  return {
    get socket() {
      return socket;
    },
    graph,
    close() {
      closed = true;
      if (reconnectTimer) clearTimeout(reconnectTimer);
      try {
        socket?.close();
      } catch {
        /* already closing */
      }
      onStatus?.(false);
    },
  };
}
