/**
 * Unit tests for quorum_parser (node --test, no DOM required).
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parseQuorumDump, EDGE_COLORS } from './quorum_parser.js';

const K = (n) => `G${String(n).padStart(55, 'A')}`;

test('shape 1: wrapped quorum_set with inner sets → direct + indirect edges', () => {
  const dump = {
    self: K(1),
    quorum_set: {
      threshold: 2,
      validators: [K(2), K(3)],
      inner_quorum_sets: [
        { threshold: 1, validators: [K(4), K(5)] },
      ],
    },
  };
  const g = parseQuorumDump(dump);
  assert.equal(g.threshold, 2);
  assert.ok(g.nodes.some((n) => n.id === K(1)));
  const kinds = (src) => g.edges.filter((e) => e.source === src);
  const k2 = kinds(K(1));
  assert.ok(k2.some((e) => e.target === K(2) && e.kind === 'direct'));
  assert.ok(k2.some((e) => e.target === K(4) && e.kind === 'indirect'));
  assert.ok(k2.every((e) => e.target !== K(1)), 'no self-loop');
});

test('shape 3: keys referenced but absent from the validator roster are missing', () => {
  const ghost = K(9);
  const dump = {
    validators: [
      { public_key: K(1), name: 'one', quorum_set: { threshold: 1, validators: [K(2), ghost] } },
      { public_key: K(2), name: 'two', quorum_set: null },
    ],
  };
  const g = parseQuorumDump(dump);
  const ghostNode = g.nodes.find((n) => n.id === ghost);
  assert.ok(ghostNode, 'ghost node created');
  assert.equal(ghostNode.missing, true);
  const edge = g.edges.find((e) => e.source === K(1) && e.target === ghost);
  assert.equal(edge.kind, 'missing');
  const known = g.edges.find((e) => e.source === K(1) && e.target === K(2));
  assert.equal(known.kind, 'direct', 'roster member stays direct');
});

test('shape 2: survey-style quorum_sets[] – every member links to every member', () => {
  const dump = {
    quorum_sets: [
      { threshold: 2, validators: [K(1), K(2), K(3)], inner_quorum_sets: [] },
    ],
  };
  const g = parseQuorumDump(dump);
  assert.equal(g.threshold, 2);
  // 3 members × 2 outgoing edges each
  const directed = g.edges.filter((e) => e.kind === 'direct').length;
  assert.equal(directed, 6);
  assert.equal(g.nodes.length, 3);
});

test('shape 3: per-validator metadata with organizations', () => {
  const dump = {
    validators: [
      { public_key: K(1), name: 'node-one', organization: 'ACME', quorum_set: { threshold: 1, validators: [K(2)] } },
      { public_key: K(2), name: 'node-two', organization: 'GLOBEX', quorum_set: null },
    ],
  };
  const g = parseQuorumDump(dump);
  const one = g.nodes.find((n) => n.id === K(1));
  assert.equal(one.name, 'node-one');
  assert.equal(one.org, 'ACME');
  const two = g.nodes.find((n) => n.id === K(2));
  assert.equal(two.org, 'GLOBEX');
  assert.equal(two.missing, false);
  assert.ok(g.edges.some((e) => e.source === K(1) && e.target === K(2) && e.kind === 'direct'));
});

test('accepts JSON string input', () => {
  const g = parseQuorumDump(JSON.stringify({
    self: K(1),
    quorum_set: { threshold: 1, validators: [K(2)] },
  }));
  assert.equal(g.nodes.length, 2);
});

test('de-duplicates repeated edges across chatty dumps', () => {
  const qs = { threshold: 1, validators: [K(2), K(2), K(3)] };
  const g = parseQuorumDump({ self: K(1), quorum_set: qs });
  const keys = g.edges.map((e) => `${e.source}>${e.target}:${e.kind}`);
  assert.equal(new Set(keys).size, keys.length);
});

test('rejects unparseable dumps with a clear error', () => {
  assert.throws(() => parseQuorumDump({ nonsense: true }), /unrecognized quorum dump/);
  assert.throws(() => parseQuorumDump(42), /must be a JSON object/);
});

test('edge colors cover all three trust kinds', () => {
  assert.equal(typeof EDGE_COLORS.direct, 'number');
  assert.equal(typeof EDGE_COLORS.indirect, 'number');
  assert.equal(typeof EDGE_COLORS.missing, 'number');
  assert.equal(new Set(Object.values(EDGE_COLORS)).size, 3, 'distinct colors');
});
