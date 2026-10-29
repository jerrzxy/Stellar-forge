/**
 * Unit tests for quorum_stream – protocol handling with an injected fake
 * WebSocket (no network, no timer flakiness).
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createQuorumStream } from './quorum_stream.js';

class FakeSocket {
  static last = null;
  constructor(url) {
    this.url = url;
    this.onopen = null;
    this.onmessage = null;
    this.onclose = null;
    this.onerror = null;
    FakeSocket.last = this;
  }
  send() {}
  close() { this.onclose?.(); }
  /** test helper */
  emit(obj) { this.onmessage?.({ data: typeof obj === 'string' ? obj : JSON.stringify(obj) }); }
}

function makeStream(overrides = {}) {
  const calls = { snapshot: [], node: [], edge: [], remove: [], status: [] };
  const stream = createQuorumStream({
    url: 'ws://test/quorum',
    WebSocketImpl: FakeSocket,
    onSnapshot: (g) => calls.snapshot.push(g),
    onNode: (n) => calls.node.push(n),
    onEdge: (e) => calls.edge.push(e),
    onRemove: (id) => calls.remove.push(id),
    onStatus: (s) => calls.status.push(s),
    ...overrides,
  });
  return { stream, calls, socket: FakeSocket.last };
}

test('snapshot message replaces graph state and fires callback', () => {
  const { calls, socket } = makeStream();
  socket.onopen?.();
  socket.emit({
    type: 'snapshot',
    nodes: [{ id: 'G1', name: 'one' }, { id: 'G2', name: 'two' }],
    edges: [{ source: 'G1', target: 'G2', kind: 'direct' }],
  });
  assert.equal(calls.snapshot.length, 1);
  assert.equal(calls.snapshot[0].nodes.length, 2);
  assert.equal(calls.snapshot[0].edges.length, 1);
});

test('node upsert merges fields', () => {
  const { calls, socket } = makeStream();
  socket.emit({ type: 'node', node: { id: 'G1', name: 'one' } });
  socket.emit({ type: 'node', node: { id: 'G1', org: 'ACME' } });
  assert.equal(calls.node.length, 2);
  assert.deepEqual(calls.node[1], { id: 'G1', name: 'one', org: 'ACME' });
});

test('edge upsert keys on source>target (reclassification)', () => {
  const { stream, calls, socket } = makeStream();
  socket.emit({ type: 'edge', edge: { source: 'GA', target: 'GB', kind: 'direct' } });
  socket.emit({ type: 'edge', edge: { source: 'GA', target: 'GB', kind: 'missing' } });
  assert.equal(calls.edge.length, 2);
  assert.equal(stream.graph.edges.size, 1);
  assert.equal(stream.graph.edges.get('GA>GB').kind, 'missing');
});

test('remove deletes node and all attached edges', () => {
  const { stream, calls, socket } = makeStream();
  socket.emit({
    type: 'snapshot',
    nodes: [{ id: 'G1' }, { id: 'G2' }, { id: 'G3' }],
    edges: [
      { source: 'G1', target: 'G2', kind: 'direct' },
      { source: 'G2', target: 'G3', kind: 'direct' },
    ],
  });
  socket.emit({ type: 'remove', id: 'G2' });
  assert.deepEqual(calls.remove, ['G2']);
  assert.equal(stream.graph.nodes.size, 2);
  assert.equal(stream.graph.edges.size, 0, 'both G2 edges dropped');
});

test('malformed and unknown frames are ignored', () => {
  const { calls, socket } = makeStream();
  socket.emit('not-json{');
  socket.emit({ type: 'flying-saucer', payload: 1 });
  socket.emit({ type: 'node', node: null });
  socket.emit({ type: 'edge', edge: { source: 'G1' } });
  assert.equal(calls.node.length, 0);
  assert.equal(calls.edge.length, 0);
  assert.equal(calls.snapshot.length, 0);
});

test('close() stops reconnection (no timer leak)', () => {
  const { stream, socket, calls } = makeStream({ reconnectMs: 5 });
  socket.onclose?.();
  stream.close();
  assert.equal(calls.status.at(-1), false, 'closed status reported');
  // After close, onclose must not schedule another connect:
  const before = FakeSocket.last;
  socket.onclose?.();
  assert.equal(FakeSocket.last, before, 'no new socket created');
});
