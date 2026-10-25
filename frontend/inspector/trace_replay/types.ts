/** Types for Soroban execution traces and diagnostic events. */

/** A single step in a Soroban execution trace. */
export interface TraceStep {
  /** Unique step identifier (monotonically increasing). */
  stepId: string;
  /** The ledger sequence number at this step. */
  ledgerSeq: number;
  /** The transaction hash that produced this step. */
  txHash: string;
  /** The event type (contract, system, or diagnostic). */
  eventType: 'contract' | 'system' | 'diagnostic';
  /** Human-readable event name or description. */
  eventName?: string;
  /** Timestamp of the event. */
  timestamp: string;
  /** Instruction count (CPU steps) at this point. */
  instructionCount: number;
  /** Current contract storage state snapshot. */
  storage: Record<string, string>;
  /** Memory allocation at this step. */
  memoryAllocation: number;
}

/** Snapshot of the ledger state at a given step. */
export interface LedgerStateSnapshot {
  /** The ledger sequence number. */
  ledgerSeq: number;
  /** The transaction hash. */
  txHash: string;
  /** Contract storage changes at this step. */
  storageChanges: Record<string, string>;
  /** Contract code modifications. */
  codeChanges: string[];
  /** Events emitted at this step. */
  events: string[];
}

/** The complete trace data structure for the replay UI. */
export interface TraceReplayData {
  /** Unique trace identifier. */
  traceId: string;
  /** The Soroban transaction hash. */
  txHash: string;
  /** Sequence of execution steps. */
  steps: TraceStep[];
  /** Final ledger state after execution. */
  finalLedgerState: LedgerStateSnapshot;
  /** Human-readable description of the trace. */
  description?: string;
}