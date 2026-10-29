import { LogLine, LogStreamConfig, LogStreamState, LogStreamStateResult, LogPatternRule } from './types';

/**
 * WebSocket-based log stream service for stellar-core logs.
 * Handles connection lifecycle, reconnection back-off, and pattern matching.
 */
export class LogStreamService {
  private readonly wsUrl: string;
  private readonly maxBuffer: number;
  private readonly initialBackoffMs: number;
  private readonly maxBackoffMs: number;

  private socket: WebSocket | null = null;
  private disposed = false;
  private backoffMs: number;

  private state: LogStreamState = 'connecting';
  private pending: LogLine[] = [];
  private rafHandle: number | null = null;

  // Pattern matching state
  private patternRules: LogPatternRule[] = [];
  private epsWindow: number[] = [];

  // Callbacks
  private stateCallbacks: ((state: LogStreamStateResult) => void)[] = [];
  private lineCallbacks: ((line: LogLine) => void)[] = [];
  private errorCallbacks: ((error: Error) => void)[] = [];

  /** Current events per second (rolling). */
  public eventsPerSecond = 0;

  /** Total lines received since connect(). */
  public totalReceived = 0;

  /** Timestamp of the last successfully received message. */
  public lastMessageAt: Date | null = null;

  constructor(options: LogStreamConfig) {
    this.wsUrl = options.wsUrl;
    this.maxBuffer = options.maxBuffer ?? 10_000;
    this.initialBackoffMs = options.initialBackoffMs ?? 1_000;
    this.maxBackoffMs = options.maxBackoffMs ?? 30_000;
    this.backoffMs = this.initialBackoffMs;

    // Set up pattern rules from options
    this.patternRules = options.defaultRules || [];

    // Rolling EPS meter
    setInterval(() => this.tickEps(), 1_000);
  }

  // ---------------------------------------------------------------------------
  // Public API
  // ---------------------------------------------------------------------------

  /** Register a callback that fires on state changes. */
  onStateChange(cb: (result: LogStreamStateResult) => void): () => void {
    this.stateCallbacks.push(cb);
    return () => { this.stateCallbacks = this.stateCallbacks.filter(c => c !== cb); };
  }

  /** Register a callback that fires on each new log line. */
  onLine(cb: (line: LogLine) => void): () => void {
    this.lineCallbacks.push(cb);
    return () => { this.lineCallbacks = this.lineCallbacks.filter(c => c !== cb); };
  }

  /** Register a callback that fires on connection errors. */
  onError(cb: (error: Error) => void): () => void {
    this.errorCallbacks.push(cb);
    return () => { this.errorCallbacks = this.errorCallbacks.filter(c => c !== cb); };
  }

  /** Open the WebSocket connection. Idempotent. */
  connect(): void {
    if (this.socket && (this.socket.readyState === WebSocket.OPEN || this.socket.readyState === WebSocket.CONNECTING)) return;

    this.disposed = false;
    this.openSocket();
  }

  /** Permanently close the connection and stop reconnecting. */
  disconnect(): void {
    this.disposed = true;
    if (this.rafHandle !== null) {
      cancelAnimationFrame(this.rafHandle);
      this.rafHandle = null;
    }
    this.socket?.close();
    this.socket = null;
    this.setState('offline');
  }

  // ---------------------------------------------------------------------------
  // Private helpers
  // ---------------------------------------------------------------------------

  private openSocket(): void {
    this.setState('connecting');
    let ws: WebSocket;
    try {
      ws = new WebSocket(this.wsUrl);
    } catch (err) {
      this.setState('error');
      this.scheduleReconnect();
      return;
    }
    this.socket = ws;

    ws.onopen = () => {
      this.backoffMs = this.initialBackoffMs;
      this.setState('live');
    };

    ws.onmessage = (ev: MessageEvent) => {
      if (this.disposed) return;
      try {
        const payload = JSON.parse(ev.data as string) as LogLine | LogLine[];
        const lines = Array.isArray(payload) ? payload : [payload];
        this.totalReceived += lines.length;
        this.lastMessageAt = new Date();
        this.enqueue(lines);
      } catch (err) {
        console.warn('[LogStreamService] Failed to parse frame', ev.data);
        const errorErr = err instanceof Error ? err : new Error('Unknown parse error');
        this.errorCallbacks.forEach(cb => cb(errorErr));
      }
    };

    ws.onerror = () => {
      this.setState('error');
    };

    ws.onclose = () => {
      if (this.disposed) return;
      this.setState('reconnecting');
      this.scheduleReconnect();
    };
  }

  private scheduleReconnect(): void {
    if (this.disposed) return;
    const delay = this.backoffMs;
    this.backoffMs = Math.min(this.backoffMs * 2, this.maxBackoffMs);
    setTimeout(() => {
      if (!this.disposed) this.openSocket();
    }, delay);
  }

  private setState(state: LogStreamState): void {
    // Debounce state changes
    if (this._state === state) return;
    this._state = state;
    this.stateCallbacks.forEach((cb) => cb(this.getStateResult()));
  }

  private get _state(): LogStreamState {
    return this.state;
  }

  private set _state(state: LogStreamState) {
    this.state = state;
  }

  /** Enqueue log lines and schedule a single rAF flush. */
  private enqueue(lines: LogLine[]): void {
    for (const line of lines) this.pending.push(line);
    if (this.rafHandle !== null) return;
    this.rafHandle = requestAnimationFrame(() => {
      this.rafHandle = null;
      const batch = this.pending.splice(0);
      if (batch.length === 0) return;
      this.processBatch(batch);
    });
  }

  /** Process a batch of log lines - apply pattern matching and dispatch. */
  private processBatch(lines: LogLine[]): void {
    // Apply pattern matching to each line
    const processed = lines.map(line => ({
      ...line,
      matchesPattern: this.patternRules.some(rule => rule.regex.test(line.message)),
      matchedPatternId: this.patternRules.find(rule => rule.regex.test(line.message))?.id,
    }));

    // Dispatch to callbacks
    processed.forEach(line => {
      this.lineCallbacks.forEach(cb => cb(line));
    });

    // Update buffer (ring-buffer)
    this.pending = [];
    
    // Trim to max buffer
    if (this.pending.length > this.maxBuffer) {
      // This shouldn't happen since we splice, but just in case
      this.pending = this.pending.slice(-this.maxBuffer);
    }

    // Update state result
    this.setState({
      state: this._state,
      logLines: [...this.pending],
      eventsPerSecond: this.eventsPerSecond,
      isConnected: this._state === 'live',
      totalReceived: this.totalReceived,
    });
  }

  /** Tick the EPS meter. */
  private tickEps(): void {
    const now = Date.now();
    const cutoff = now - 1_000;
    let i = 0;
    while (i < this.epsWindow.length && this.epsWindow[i] < cutoff) i++;
    this.epsWindow = this.epsWindow.slice(i);
    this.eventsPerSecond = this.epsWindow.length;
  }

  /** Get current state result. */
  private getStateResult(): LogStreamStateResult {
    return {
      state: this._state,
      logLines: [...this.pending],
      eventsPerSecond: this.eventsPerSecond,
      isConnected: this._state === 'live',
      totalReceived: this.totalReceived,
    };
  }

  /** Get current state (read-only externally). */
  get state(): LogStreamState {
    return this._state;
  }

  /** Get the current buffer of log lines (read-only). */
  get lines(): LogLine[] {
    return [...this.pending];
  }
}

/** React hook that connects to the log stream and returns state. */
import { useEffect, useRef, useState } from 'react';

export interface UseLogStreamResult {
  state: LogStreamState;
  logLines: LogLine[];
  eventsPerSecond: number;
  isConnected: boolean;
  totalReceived: number;
  totalReceivedDisplay: string;
  connect: () => void;
  disconnect: () => void;
  clearLines: () => void;
}

/** React hook to connect to the log stream. */
export function useLogStream(service: LogStreamService, capacity = 10_000): UseLogStreamResult {
  const [state, setState] = useState<LogStreamState>(service.state);
  const [logLines, setLogLines] = useState<LogLine[]>([]);
  const [eps, setEps] = useState(0);
  const [total, setTotal] = useState(0);
  const [lastAt, setLastAt] = useState<Date | null>(null);
  const [totalReceivedDisplay, setTotalReceivedDisplay] = useState('0');

  // Ring-buffer stored in a ref
  const bufRef = useRef<LogLine[]>([]);

  // rAF handle for coalescing state flushes.
  const rafRef = useRef<number | null>(null);

  useEffect(() => {
    const unsubState = service.onStateChange((result) => {
      setState(result.state);
      setLogLines(result.logLines);
      setEps(result.eventsPerSecond);
      setTotal(result.totalReceived);
      setLastAt(result.lastMessageAt);
      setTotalReceivedDisplay(result.totalReceived.toString());
    });

    const unsubLine = service.onLine((line) => {
      // Prepend newest events, trim to capacity
      bufRef.current = [...line, ...bufRef.current].slice(0, capacity);
      // Coalesce state updates into a single rAF.
      if (rafRef.current !== null) return;
      rafRef.current = requestAnimationFrame(() => {
        rafRef.current = null;
        setLogLines([...bufRef.current]);
      });
    });

    const unsubError = service.onError((err) => {
      console.error('[LogStream] Error:', err.message);
    });

    return () => {
      unsubState();
      unsubLine();
      unsubError();
      if (rafRef.current !== null) cancelAnimationFrame(rafRef.current);
    };
  }, [service, capacity]);

  const connect = () => service.connect();
  const disconnect = () => service.disconnect();
  const clearLines = () => {
    bufRef.current = [];
    setLogLines([]);
  };

  return {
    state,
    logLines,
    eventsPerSecond: eps,
    isConnected: state === 'live',
    totalReceived,
    totalReceivedDisplay,
    connect,
    disconnect,
    clearLines,
  };
}