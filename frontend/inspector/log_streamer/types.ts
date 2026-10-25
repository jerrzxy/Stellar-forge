/** Types for Real-Time Validator Log Streamer & Pattern Matcher */

/** A single log line from stellar-core. */
export interface LogLine {
  /** Unique identifier for the log line. */
  id: string;
  /** ISO-8601 timestamp when the log was emitted. */
  timestamp: string;
  /** The raw log message from stellar-core. */
  message: string;
  /** The severity level of the log. */
  level: 'info' | 'warning' | 'error' | 'critical';
  /** The component or subsystem that generated the log. */
  component: string;
  /** Whether this line matches any active pattern. */
  matchesPattern: boolean;
  /** The pattern ID that matched, if any. */
  matchedPatternId?: string;
}

/** A rule for highlighting log patterns. */
export interface LogPatternRule {
  /** Unique identifier for the pattern rule. */
  id: string;
  /** Human-readable name for the rule. */
  name: string;
  /** Regular expression to match against log messages. */
  regex: RegExp;
  /** CSS class for highlighting matched logs. */
  highlightClass: string;
  /** Human-readable description of what this rule catches. */
  description: string;
  /** Whether the rule is enabled. */
  enabled: boolean;
}

/** Configuration for the log streamer. */
export interface LogStreamConfig {
  /** WebSocket URL for the logging proxy. */
  wsUrl: string;
  /** Default pattern rules to load. */
  defaultRules: LogPatternRule[];
  /** Maximum number of log lines to retain in memory. */
  maxBuffer: number;
  /** Reconnection initial back-off in ms. */
  initialBackoffMs?: number;
  /** Maximum reconnection back-off in ms. */
  maxBackoffMs?: number;
}

/** State of the log streamer. */
export enum LogStreamState {
  /** Connecting to the WebSocket. */
  Connecting,
  /** Connection established and streaming. */
  Live,
  /** Temporarily offline, reconnecting. */
  Reconnecting,
  /** Offline and not reconnecting. */
  Offline,
  /** Error state. */
  Error,
}

/** Result of pattern matching on a log line. */
export interface PatternMatchResult {
  /** Whether any pattern matched. */
  matched: boolean;
  /** The ID of the matching pattern, or undefined. */
  patternId?: string;
  /** The captured groups from the regex match. */
  groups?: Record<string, string>;
}

/** Props for the VirtualLogList component. */
export interface VirtualLogListProps {
  /** The log lines to display. */
  logLines: LogLine[];
  /** The pattern rules for highlighting. */
  patternRules: LogPatternRule[];
  /** Callback when a log line is clicked. */
  onLineClick?: (line: LogLine) => void;
  /** Whether to auto-scroll to newest lines. */
  autoScroll?: boolean;
}

/** State update from the WebSocket service. */
export interface LogStreamStateResult {
  /** Current state of the stream. */
  state: LogStreamState;
  /** The current buffer of log lines. */
  logLines: LogLine[];
  /** Current events per second. */
  eventsPerSecond: number;
  /** Whether the stream is connected. */
  isConnected: boolean;
  /** Total lines received since connect. */
  totalReceived: number;
}