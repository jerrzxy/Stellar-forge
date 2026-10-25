/**
 * Unit tests for LogViewer filtering and empty-state logic.
 *
 * Validates:
 *  1. Normalizes string logs and structured LogEntry objects.
 *  2. Evaluates empty state when search query returns zero results.
 *  3. Verifies default empty state title is "No matching log entries found".
 *  4. Evaluates empty state description contains active search query.
 *  5. Verifies empty state is suppressed during loading.
 *  6. Verifies empty state is suppressed when error occurs.
 *  7. Filters logs accurately by keyword, level, and source.
 *  8. Evaluates empty state when log stream has 0 items.
 *
 * Run with: node --test frontend/logs/tests/logViewer.test.js
 */

import test from 'node:test';
import assert from 'node:assert/strict';

import {
  normalizeLog,
  filterLogs,
  evaluateEmptyState,
} from '../utils/logUtils.js';

const SAMPLE_LOGS = [
  '2026-10-21T10:00:00Z [INFO] stellar-core node initialized successfully',
  '2026-10-21T10:00:05Z [WARN] slow ledger catchup detected on validator-1',
  '2026-10-21T10:00:10Z [ERROR] connection timed out to peer 192.168.1.42',
  '2026-10-21T10:00:15Z [DEBUG] gossip message broadcast to 12 peers',
  {
    id: 'entry-5',
    timestamp: '2026-10-21T10:00:20Z',
    level: 'info',
    source: 'horizon-db',
    message: 'Replication lag within normal boundary (12ms)',
  },
];

test('normalizes string logs and infers log level', () => {
  const normError = normalizeLog('CRITICAL [ERROR] DB connection closed', 0);
  assert.equal(normError.level, 'error');
  assert.equal(normError.message, 'CRITICAL [ERROR] DB connection closed');

  const normWarn = normalizeLog('[WARN] memory high', 1);
  assert.equal(normWarn.level, 'warn');

  const normInfo = normalizeLog('regular info message', 2);
  assert.equal(normInfo.level, 'info');
});

test('normalizes structured LogEntry preserving metadata', () => {
  const entry = {
    id: 'custom-123',
    level: 'fatal',
    source: 'soroban-rpc',
    message: 'WASM out of gas',
  };
  const norm = normalizeLog(entry, 0);
  assert.equal(norm.id, 'custom-123');
  assert.equal(norm.level, 'fatal');
  assert.equal(norm.source, 'soroban-rpc');
  assert.equal(norm.message, 'WASM out of gas');
});

test('filters logs by search query (case-insensitive)', () => {
  const matches = filterLogs(SAMPLE_LOGS, 'validator-1');
  assert.equal(matches.length, 1);
  assert.ok(matches[0].message.includes('validator-1'));

  const zeroMatches = filterLogs(SAMPLE_LOGS, 'non-existent-string-xyz');
  assert.equal(zeroMatches.length, 0);
});

test('filters logs by source and level keywords', () => {
  const matchesSource = filterLogs(SAMPLE_LOGS, 'horizon-db');
  assert.equal(matchesSource.length, 1);
  assert.equal(matchesSource[0].source, 'horizon-db');

  const errorMatches = filterLogs(SAMPLE_LOGS, undefined, 'error');
  assert.equal(errorMatches.length, 1);
  assert.equal(errorMatches[0].level, 'error');
});

test('triggers empty state when search returns zero results', () => {
  const query = 'unexpected-panic';
  const filtered = filterLogs(SAMPLE_LOGS, query);
  assert.equal(filtered.length, 0);

  const emptyState = evaluateEmptyState({
    rawLogs: SAMPLE_LOGS,
    filteredLogs: filtered,
    searchQuery: query,
  });

  assert.equal(emptyState.shouldShowEmptyState, true);
  assert.equal(emptyState.title, 'No matching log entries found');
  assert.ok(emptyState.description.includes(query));
  assert.equal(emptyState.hasActiveSearch, true);
});

test('triggers empty state when initial logs array is empty', () => {
  const emptyState = evaluateEmptyState({
    rawLogs: [],
    filteredLogs: [],
    searchQuery: '',
  });

  assert.equal(emptyState.shouldShowEmptyState, true);
  assert.equal(emptyState.title, 'No matching log entries found');
  assert.equal(emptyState.hasActiveSearch, false);
});

test('does NOT trigger empty state when matching logs exist', () => {
  const query = 'stellar-core';
  const filtered = filterLogs(SAMPLE_LOGS, query);
  assert.ok(filtered.length > 0);

  const emptyState = evaluateEmptyState({
    rawLogs: SAMPLE_LOGS,
    filteredLogs: filtered,
    searchQuery: query,
  });

  assert.equal(emptyState.shouldShowEmptyState, false);
});

test('does NOT trigger empty state during loading', () => {
  const emptyState = evaluateEmptyState({
    rawLogs: [],
    filteredLogs: [],
    searchQuery: 'something',
    loading: true,
  });

  assert.equal(emptyState.shouldShowEmptyState, false);
});

test('does NOT trigger empty state when error is present', () => {
  const emptyState = evaluateEmptyState({
    rawLogs: [],
    filteredLogs: [],
    searchQuery: 'something',
    error: new Error('Stream disconnect'),
  });

  assert.equal(emptyState.shouldShowEmptyState, false);
});

test('respects custom title and description overrides', () => {
  const emptyState = evaluateEmptyState({
    rawLogs: SAMPLE_LOGS,
    filteredLogs: [],
    searchQuery: 'foo',
    customTitle: 'Zero Results Found',
    customDescription: 'Custom explanatory guidance.',
  });

  assert.equal(emptyState.title, 'Zero Results Found');
  assert.equal(emptyState.description, 'Custom explanatory guidance.');
});
