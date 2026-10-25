import { parseSorobanTrace } from '../../analytics/src/profiler/flamegraph/traceParser';
import { TraceStep, TraceReplayData, LedgerStateSnapshot } from './types';

/**
 * Parse raw Soroban trace data into a structured TraceReplayData object.
 * This enables step-by-step debugging of failed or complex smart contract calls.
 *
 * @param rawTraceData Raw trace data from Soroban RPC or flamegraph format
 * @returns Structured trace data ready for the trace replay UI
 */
export function parseTraceForReplay(rawTraceData: any): TraceReplayData {
  const parsedTraces = parseSorobanTrace(rawTraceData);

  if (parsedTraces.length === 0) {
    return {
      traceId: '',
      txHash: '',
      steps: [],
      finalLedgerState: {
        ledgerSeq: 0,
        txHash: '',
        storageChanges: {},
        codeChanges: [],
        events: [],
      },
      description: 'No trace data available',
    };
  }

  // Extract transaction hash from the first trace
  const txHash = parsedTraces[0].contractId || rawTraceData?.id || rawTraceData?.txHash || '';

  // Build steps from each trace entry
  const steps: TraceStep[] = parsedTraces.map((trace, index) => ({
    stepId: `step-${index}`,
    ledgerSeq: trace.ledger ?? trace.ledgerSeq ?? index,
    txHash,
    eventType: trace.eventType || 'contract',
    eventName: trace.contractName || trace.name || `Step ${index + 1}`,
    timestamp: trace.timestamp || new Date().toISOString(),
    instructionCount: trace.instructionCount || trace.gasUsed || 0,
    storage: parseStorageSnapshot(trace),
    memoryAllocation: parseMemoryAllocation(trace),
  }));

  // Build final ledger state from the last step
  const lastStep = steps[steps.length - 1];
  const finalLedgerState: LedgerStateSnapshot = {
    ledgerSeq: lastStep.ledgerSeq,
    txHash,
    storageChanges: lastStep.storage,
    codeChanges: [], // Would need deeper trace analysis
    events: [], // Would need event extraction
  };

  return {
    traceId: parsedTraces[0].contractId || '',
    txHash,
    steps,
    finalLedgerState,
    description: rawTraceData?.description || rawTraceData?.name || 'Soroban Transaction Trace',
  };
}

/** Extract storage snapshot from a trace entry. */
function parseStorageSnapshot(trace: any): Record<string, string> {
  // Try various property names for storage data
  const storage = trace.storage || trace.stateDelta || trace.storageChanges || {};

  if (typeof storage === 'object' && storage !== null) {
    // Convert to key-value record
    const result: Record<string, string> = {};
    Object.entries(storage).forEach(([key, value]) => {
      result[String(key)] = String(value || '');
    });
    return result;
  }

  return {};
}

/** Extract memory allocation from a trace entry. */
function parseMemoryAllocation(trace: any): number {
  // Try various property names for memory data
  const memory = trace.memoryAllocation || trace.memory || trace.cpu || trace.instructionCount;

  if (typeof memory === 'number') {
    return memory;
  }

  if (typeof memory === 'string') {
    // Try to parse numeric value
    const parsed = parseFloat(memory);
    if (!isNaN(parsed)) return parsed;
  }

  return 0;
}

/**
 * Create TraceReplayData from Soroban event stream events.
 * This is useful when you have a live event stream rather than a pre-recorded trace.
 */
export function createTraceFromEvents(events: any[]): TraceReplayData {
  // Sort events by ledger sequence (or timestamp) to create a timeline
  const sortedEvents = [...events].sort((a, b) => {
    const seqA = a.ledger ?? a.timestamp ?? 0;
    const seqB = b.ledger ?? b.timestamp ?? 0;
    return Number(seqA) - Number(seqB);
  });

  // Build steps from events, grouping consecutive events with the same txHash
  const steps: TraceStep[] = sortedEvents.map((event, index) => ({
    stepId: `step-${index}`,
    ledgerSeq: event.ledger ?? index,
    txHash: event.tx_hash || '',
    eventType: event.event_type || 'contract',
    eventName: event.topics?.[0] || event.eventName || `Event ${index + 1}`,
    timestamp: event.timestamp || new Date().toISOString(),
    instructionCount: event.instruction_count || event.instructionCount || 0,
    storage: parseStorageFromEvent(event),
    memoryAllocation: 0, // Not typically available in event stream
  }));

  return {
    traceId: sortedEvents[0]?.contract_id || '',
    txHash: sortedEvents[0]?.tx_hash || '',
    steps,
    finalLedgerState: {
      ledgerSeq: sortedEvents[sortedEvents.length - 1]?.ledger ?? 0,
      txHash: sortedEvents[0]?.tx_hash || '',
      storageChanges: {},
      codeChanges: [],
      events: sortedEvents.map(e => e.id || e.hash || ''),
    },
    description: 'Trace from event stream',
  };
}

/** Extract storage from a single event. */
function parseStorageFromEvent(event: any): Record<string, string> {
  const valueXdr = event.value_xdr;

  if (!valueXdr) {
    return {};
  }

  // In a full implementation, we would decode the XDR value
  // For now, return empty storage
  return {};
}