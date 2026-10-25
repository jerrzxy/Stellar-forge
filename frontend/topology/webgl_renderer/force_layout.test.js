/**
 * Unit tests for force_layout – deterministic stepping, no browser.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createForceLayout, LAYOUT_DEFAULTS } from './force_layout.js';
import { parseQuorumDump } from './quorum_parser.js';

function starGraph(n) {
  const edges = [];
  for (let i = 1; i < n; i++) edges.push({ source: 'G0', target: `G${i}`, kind: 'direct' });
  return parseQuorumDump({
    self: 'G0',
    quorum_set: { threshold: 1, validators: edges.map((e) => e.target) },
  });
}

test('nodes get deterministic initial positions', () => {
  const a = starGraph(50);
  const b = starGraph(50);
  createForceLayout(a).stop();
  createForceLayout(b).stop();
  for (let i = 0; i < a.nodes.length; i++) {
    assert.equal(a.nodes[i].x, b.nodes[i].x);
    assert.equal(a.nodes[i].y, b.nodes[i].y);
    assert.equal(a.nodes[i].z, b.nodes[i].z);
  }
});

test('tick() advances physics and returns hot state', () => {
  const g = starGraph(30);
  const layout = createForceLayout(g);
  const before = g.nodes.map((n) => [n.x, n.y, n.z]);
  const hot = layout.tick(5);
  assert.equal(typeof hot, 'boolean');
  const moved = g.nodes.some((n, i) =>
    n.x !== before[i][0] || n.y !== before[i][1] || n.z !== before[i][2]);
  assert.ok(moved, 'nodes moved after ticks');
  layout.stop();
});

test('tick() never exceeds maxTicksPerFrame (jank guard)', () => {
  const g = starGraph(20);
  const layout = createForceLayout(g);
  const alphaBefore = layout.alpha();
  layout.tick(10_000); // ask for absurd count – clamped to cap
  // alphaDecay per tick is bounded; 3 ticks can only drop alpha modestly
  const maxDrop = LAYOUT_DEFAULTS.maxTicksPerFrame * LAYOUT_DEFAULTS.alphaDecay + 1e-9;
  assert.ok(alphaBefore - layout.alpha() <= maxDrop + 0.05,
    'alpha drop bounded by maxTicksPerFrame');
  layout.stop();
});

test('layout cools down over many ticks (simulation converges)', () => {
  const g = starGraph(40);
  const layout = createForceLayout(g);
  let steps = 0;
  while (layout.tick() && steps < 5000) steps++;
  assert.ok(steps < 5000, 'converged before hard stop');
  assert.ok(layout.alpha() <= layout.simulation.alphaMin() + 1e-9);
  layout.stop();
});

test('reheat() resumes the simulation', () => {
  const g = starGraph(20);
  const layout = createForceLayout(g);
  while (layout.tick()) { /* cool down */ }
  assert.equal(layout.tick(), false);
  layout.reheat(0.8);
  assert.ok(layout.alpha() > 0.5);
  layout.stop();
});
