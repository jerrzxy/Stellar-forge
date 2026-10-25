/**
 * Tests for the mainnet snapshot generator + its round-trip through the
 * parser (this is the exact payload the demo/validation loads).
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { buildMainnetSnapshot } from './mainnet_snapshot.js';
import { parseQuorumDump } from './quorum_parser.js';

test('snapshot is deterministic for a given seed', () => {
  const a = buildMainnetSnapshot({ orgs: 10, validatorsPerOrg: 5 });
  const b = buildMainnetSnapshot({ orgs: 10, validatorsPerOrg: 5 });
  assert.deepEqual(a, b);
  const c = buildMainnetSnapshot({ orgs: 10, validatorsPerOrg: 5, seed: 7 });
  assert.notEqual(JSON.stringify(a), JSON.stringify(c), 'different seed differs');
});

test('snapshot parses into a connected graph with all edge kinds present', () => {
  const snap = buildMainnetSnapshot({ orgs: 20, validatorsPerOrg: 8 });
  const graph = parseQuorumDump(snap);
  assert.ok(graph.nodes.length >= snap.validators.length,
    'all roster validators present (+ ghost nodes for missing peers)');

  const kinds = new Set(graph.edges.map((e) => e.kind));
  assert.ok(kinds.has('direct'), 'direct trust edges exist');
  assert.ok(kinds.has('indirect'), 'indirect trust edges exist');
  assert.ok(kinds.has('missing'), 'missing peer edges exist');

  // Every edge endpoint resolves to a node
  const ids = new Set(graph.nodes.map((n) => n.id));
  for (const e of graph.edges) {
    assert.ok(ids.has(e.source) && ids.has(e.target));
  }
});

test('missing nodes are flagged for the renderer', () => {
  const snap = buildMainnetSnapshot({ orgs: 20, validatorsPerOrg: 8, missingFraction: 0.1 });
  const graph = parseQuorumDump(snap);
  const missing = graph.nodes.filter((n) => n.missing);
  assert.ok(missing.length > 0, 'at least one missing peer at 10% fraction');
  assert.ok(missing.every((n) => n.name === n.id), 'ghost nodes have no friendly name');
});
