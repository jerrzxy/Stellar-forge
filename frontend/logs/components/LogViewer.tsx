import React, {
  forwardRef,
  useCallback,
  useEffect,
  useImperativeHandle,
  useMemo,
  useRef,
  useState,
} from 'react';

// ─── Types ────────────────────────────────────────────────────────────────────

export type LogLevel =
  | 'trace'
  | 'debug'
  | 'info'
  | 'warn'
  | 'warning'
  | 'error'
  | 'fatal'
  | string;

export interface LogEntry {
  id?: string | number;
  timestamp?: string | number | Date;
  level?: LogLevel;
  message: string;
  source?: string;
  metadata?: Record<string, unknown>;
  [key: string]: unknown;
}

export type LogItem = string | LogEntry;

export interface LogEmptyStateProps {
  /** The search query that returned no results */
  searchQuery?: string;
  /** Callback to clear the search input or filters */
  onClearSearch?: () => void;
  /** Heading displayed in the empty state */
  title?: string;
  /** Explanatory description */
  description?: string;
  /** Custom icon or illustration */
  icon?: React.ReactNode;
  /** Custom action button text */
  actionLabel?: string;
  /** Additional CSS class */
  className?: string;
  /** Inline styles */
  style?: React.CSSProperties;
}

export interface LogViewerProps {
  /** Array of log items (strings or structured LogEntry objects) */
  logs?: LogItem[];
  /** Alias for logs */
  lines?: LogItem[];
  /** Search query to filter log messages */
  searchQuery?: string;
  /** Alias for searchQuery */
  searchTerm?: string;
  /** Alias for searchQuery */
  filter?: string;
  /** Filter logs by level (e.g. 'error', 'warn', 'info', 'debug') */
  levelFilter?: string;
  /** Whether logs are actively streaming or loading */
  loading?: boolean;
  /** Alias for loading */
  isLoading?: boolean;
  /** Error message or error object if log stream encountered a failure */
  error?: string | Error | null;
  /** Callback fired when user requests to clear search */
  onClearSearch?: () => void;
  /** Callback fired when search input value changes */
  onSearchChange?: (query: string) => void;
  /** Custom component or element to render when no matching logs exist */
  emptyState?: React.ReactNode;
  /** Custom title for the empty state */
  emptyStateTitle?: string;
  /** Custom description for the empty state */
  emptyStateDescription?: string;
  /** Whether to render the integrated search bar */
  showSearchBar?: boolean;
  /** Placeholder text for the search input */
  searchPlaceholder?: string;
  /** Whether to display line numbers */
  showLineNumbers?: boolean;
  /** Whether to display timestamps when available */
  showTimestamps?: boolean;
  /** Whether to automatically scroll to bottom on new log entries */
  autoScroll?: boolean;
  /** Title header for the viewer */
  title?: string;
  /** Maximum height for the scrollable log window */
  maxHeight?: number | string;
  /** Additional CSS class */
  className?: string;
  /** Inline styles */
  style?: React.CSSProperties;
}

export interface LogViewerRef {
  scrollToBottom: () => void;
  scrollToTop: () => void;
  clearSearch: () => void;
  getFilteredLogs: () => LogItem[];
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

function normalizeLogEntry(item: LogItem, index: number): LogEntry {
  if (typeof item === 'string') {
    let level: LogLevel = 'info';
    const upper = item.toUpperCase();
    if (upper.includes('FATAL')) level = 'fatal';
    else if (upper.includes('ERROR') || upper.includes('ERR')) level = 'error';
    else if (upper.includes('WARN')) level = 'warn';
    else if (upper.includes('DEBUG')) level = 'debug';
    else if (upper.includes('TRACE')) level = 'trace';

    return {
      id: index,
      message: item,
      level,
    };
  }
  return {
    id: item.id ?? index,
    message: item.message ?? '',
    level: item.level || 'info',
    timestamp: item.timestamp,
    source: item.source,
    metadata: item.metadata,
  };
}

function getLevelBadgeStyle(level?: string): { color: string; bg: string; border: string } {
  const norm = (level || 'info').toLowerCase();
  switch (norm) {
    case 'error':
    case 'fatal':
    case 'err':
      return { color: '#ef4444', bg: 'rgba(239, 68, 68, 0.1)', border: 'rgba(239, 68, 68, 0.25)' };
    case 'warn':
    case 'warning':
      return { color: '#f59e0b', bg: 'rgba(245, 158, 11, 0.1)', border: 'rgba(245, 158, 11, 0.25)' };
    case 'debug':
    case 'trace':
      return { color: '#8b5cf6', bg: 'rgba(139, 92, 246, 0.1)', border: 'rgba(139, 92, 246, 0.25)' };
    case 'info':
    default:
      return { color: '#3b82f6', bg: 'rgba(59, 130, 246, 0.1)', border: 'rgba(59, 130, 246, 0.25)' };
  }
}

// ─── LogEmptyState Component ──────────────────────────────────────────────────

/**
 * Friendly empty state component displayed when log search queries return zero results.
 * Clarifies to users that the log stream rendered successfully, but no entries matched.
 */
export const LogEmptyState: React.FC<LogEmptyStateProps> = ({
  searchQuery,
  onClearSearch,
  title = 'No matching log entries found',
  description,
  icon,
  actionLabel = 'Clear search filter',
  className = '',
  style,
}) => {
  const defaultDescription = searchQuery
    ? `No log lines match your query "${searchQuery}". Try revising your terms or resetting active filters.`
    : 'No matching log entries were found matching your current filter criteria. Check your query syntax or widen your search scope.';

  return (
    <div
      role="status"
      aria-live="polite"
      className={`log-empty-state ${className}`.trim()}
      data-testid="log-empty-state"
      style={{
        display: 'flex',
        flexDirection: 'column',
        alignItems: 'center',
        justifyContent: 'center',
        padding: '48px 24px',
        textAlign: 'center',
        color: '#94a3b8',
        backgroundColor: 'rgba(15, 23, 42, 0.35)',
        borderRadius: '8px',
        border: '1px dashed #334155',
        margin: '16px',
        minHeight: '220px',
        ...style,
      }}
    >
      <div
        className="log-empty-state-icon"
        aria-hidden="true"
        style={{
          width: '52px',
          height: '52px',
          borderRadius: '50%',
          backgroundColor: 'rgba(51, 65, 85, 0.5)',
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'center',
          marginBottom: '16px',
          color: '#38bdf8',
        }}
      >
        {icon || (
          <svg
            width="26"
            height="26"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <circle cx="11" cy="11" r="8" />
            <line x1="21" y1="21" x2="16.65" y2="16.65" />
            <line x1="8" y1="11" x2="14" y2="11" />
          </svg>
        )}
      </div>

      <h3
        className="log-empty-state-title"
        data-testid="log-empty-state-title"
        style={{
          margin: '0 0 8px 0',
          fontSize: '16px',
          fontWeight: 600,
          color: '#f1f5f9',
          letterSpacing: '-0.01em',
        }}
      >
        {title}
      </h3>

      <p
        className="log-empty-state-description"
        data-testid="log-empty-state-description"
        style={{
          margin: '0 0 16px 0',
          fontSize: '13px',
          lineHeight: 1.5,
          color: '#94a3b8',
          maxWidth: '420px',
        }}
      >
        {description || defaultDescription}
      </p>

      {onClearSearch && (
        <button
          type="button"
          onClick={onClearSearch}
          className="log-empty-state-action-btn"
          data-testid="log-empty-state-clear-btn"
          style={{
            display: 'inline-flex',
            alignItems: 'center',
            gap: '6px',
            padding: '7px 14px',
            fontSize: '13px',
            fontWeight: 500,
            color: '#38bdf8',
            backgroundColor: 'rgba(56, 189, 248, 0.1)',
            border: '1px solid rgba(56, 189, 248, 0.25)',
            borderRadius: '6px',
            cursor: 'pointer',
            transition: 'background-color 0.15s ease, border-color 0.15s ease',
          }}
        >
          <svg
            width="14"
            height="14"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <line x1="18" y1="6" x2="6" y2="18" />
            <line x1="6" y1="6" x2="18" y2="18" />
          </svg>
          {actionLabel}
        </button>
      )}
    </div>
  );
};

// ─── Main LogViewer Component ─────────────────────────────────────────────────

/**
 * LogViewer component for rendering streaming logs with search, filtering,
 * level coloring, and a friendly empty state when no matching entries are found.
 */
export const LogViewer = forwardRef<LogViewerRef, LogViewerProps>(function LogViewer(
  {
    logs,
    lines,
    searchQuery: controlledSearchQuery,
    searchTerm,
    filter,
    levelFilter = 'all',
    loading = false,
    isLoading = false,
    error = null,
    onClearSearch,
    onSearchChange,
    emptyState,
    emptyStateTitle = 'No matching log entries found',
    emptyStateDescription,
    showSearchBar = true,
    searchPlaceholder = 'Filter logs by keyword, regex, or status…',
    showLineNumbers = true,
    showTimestamps = true,
    autoScroll = true,
    title,
    maxHeight = 520,
    className = '',
    style,
  },
  ref,
) {
  // Unify logs / lines props
  const rawLogs: LogItem[] = useMemo(() => logs || lines || [], [logs, lines]);

  // Unify loading & error props
  const isBusy = loading || isLoading;
  const errorText = error ? (error instanceof Error ? error.message : String(error)) : null;

  // Search state management (controlled vs internal)
  const initialQuery = controlledSearchQuery ?? searchTerm ?? filter ?? '';
  const [internalQuery, setInternalQuery] = useState<string>(initialQuery);
  const isControlledSearch = controlledSearchQuery !== undefined;
  const activeSearchQuery = isControlledSearch ? controlledSearchQuery : internalQuery;

  // Sync internal state if controlled query changes
  useEffect(() => {
    if (isControlledSearch) {
      setInternalQuery(controlledSearchQuery);
    }
  }, [isControlledSearch, controlledSearchQuery]);

  const containerRef = useRef<HTMLDivElement | null>(null);
  const scrollAreaRef = useRef<HTMLDivElement | null>(null);

  // Normalize logs
  const normalizedEntries = useMemo(() => {
    return rawLogs.map((item, idx) => normalizeLogEntry(item, idx));
  }, [rawLogs]);

  // Filter logs by search query & level
  const filteredEntries = useMemo(() => {
    let result = normalizedEntries;

    // Filter by level
    if (levelFilter && levelFilter !== 'all') {
      const targetLevel = levelFilter.toLowerCase();
      result = result.filter((entry) => (entry.level || '').toLowerCase() === targetLevel);
    }

    // Filter by search query
    if (activeSearchQuery.trim()) {
      const q = activeSearchQuery.trim().toLowerCase();
      result = result.filter((entry) => {
        const msg = entry.message.toLowerCase();
        const src = (entry.source || '').toLowerCase();
        const lvl = (entry.level || '').toLowerCase();
        return msg.includes(q) || src.includes(q) || lvl.includes(q);
      });
    }

    return result;
  }, [normalizedEntries, activeSearchQuery, levelFilter]);

  // Auto-scroll on new entries
  useEffect(() => {
    if (autoScroll && scrollAreaRef.current) {
      scrollAreaRef.current.scrollTop = scrollAreaRef.current.scrollHeight;
    }
  }, [filteredEntries.length, autoScroll]);

  // Clear search handler
  const handleClearSearch = useCallback(() => {
    setInternalQuery('');
    onClearSearch?.();
    onSearchChange?.('');
  }, [onClearSearch, onSearchChange]);

  const handleSearchInputChange = (e: React.ChangeEvent<HTMLInputElement>) => {
    const val = e.target.value;
    if (!isControlledSearch) {
      setInternalQuery(val);
    }
    onSearchChange?.(val);
  };

  // Expose imperative API via ref
  useImperativeHandle(
    ref,
    () => ({
      scrollToBottom: () => {
        if (scrollAreaRef.current) {
          scrollAreaRef.current.scrollTop = scrollAreaRef.current.scrollHeight;
        }
      },
      scrollToTop: () => {
        if (scrollAreaRef.current) {
          scrollAreaRef.current.scrollTop = 0;
        }
      },
      clearSearch: handleClearSearch,
      getFilteredLogs: () => filteredEntries,
    }),
    [filteredEntries, handleClearSearch],
  );

  const hasSearchFilter = Boolean(activeSearchQuery.trim() || (levelFilter && levelFilter !== 'all'));
  const hasZeroResults = filteredEntries.length === 0;

  return (
    <div
      ref={containerRef}
      className={`log-viewer-container ${className}`.trim()}
      data-testid="log-viewer"
      style={{
        display: 'flex',
        flexDirection: 'column',
        width: '100%',
        backgroundColor: '#0b0f19',
        border: '1px solid #1e293b',
        borderRadius: '8px',
        overflow: 'hidden',
        color: '#e2e8f0',
        fontFamily: 'ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", "Courier New", monospace',
        fontSize: '13px',
        boxShadow: '0 4px 6px -1px rgba(0, 0, 0, 0.2)',
        ...style,
      }}
    >
      {/* Optional Header / Title bar */}
      {(title || showSearchBar) && (
        <div
          className="log-viewer-header"
          style={{
            padding: '10px 14px',
            backgroundColor: '#111827',
            borderBottom: '1px solid #1e293b',
            display: 'flex',
            flexWrap: 'wrap',
            alignItems: 'center',
            justifyContent: 'space-between',
            gap: '10px',
          }}
        >
          {title && (
            <div
              className="log-viewer-title"
              style={{
                fontWeight: 600,
                fontSize: '14px',
                color: '#f8fafc',
                display: 'flex',
                alignItems: 'center',
                gap: '8px',
              }}
            >
              <span
                style={{
                  width: '8px',
                  height: '8px',
                  borderRadius: '50%',
                  backgroundColor: isBusy ? '#f59e0b' : '#10b981',
                  display: 'inline-block',
                }}
              />
              {title}
              <span
                style={{
                  fontSize: '11px',
                  color: '#64748b',
                  fontWeight: 400,
                  marginLeft: '4px',
                }}
              >
                ({filteredEntries.length} / {rawLogs.length} lines)
              </span>
            </div>
          )}

          {showSearchBar && (
            <div
              className="log-viewer-search-wrapper"
              style={{
                display: 'flex',
                alignItems: 'center',
                position: 'relative',
                flexGrow: title ? 0 : 1,
                minWidth: '220px',
              }}
            >
              <span
                style={{
                  position: 'absolute',
                  left: '10px',
                  color: '#64748b',
                  pointerEvents: 'none',
                  display: 'flex',
                }}
              >
                🔍
              </span>
              <input
                type="text"
                value={activeSearchQuery}
                onChange={handleSearchInputChange}
                placeholder={searchPlaceholder}
                className="log-viewer-search-input"
                data-testid="log-viewer-search-input"
                style={{
                  width: '100%',
                  padding: '6px 30px 6px 32px',
                  backgroundColor: '#0b0f19',
                  border: '1px solid #334155',
                  borderRadius: '6px',
                  color: '#f8fafc',
                  fontSize: '12px',
                  outline: 'none',
                  transition: 'border-color 0.15s ease',
                }}
              />
              {activeSearchQuery && (
                <button
                  type="button"
                  onClick={handleClearSearch}
                  aria-label="Clear search input"
                  style={{
                    position: 'absolute',
                    right: '6px',
                    top: '50%',
                    transform: 'translateY(-50%)',
                    background: 'none',
                    border: 'none',
                    color: '#94a3b8',
                    cursor: 'pointer',
                    fontSize: '14px',
                    padding: '2px 4px',
                    lineHeight: 1,
                  }}
                >
                  ✕
                </button>
              )}
            </div>
          )}
        </div>
      )}

      {/* Main Content Area */}
      <div
        ref={scrollAreaRef}
        className="log-viewer-scroll-area"
        data-testid="log-viewer-scroll-area"
        style={{
          maxHeight,
          overflowY: 'auto',
          overflowX: 'auto',
          backgroundColor: '#090d16',
          position: 'relative',
          minHeight: '180px',
        }}
      >
        {/* Error Banner */}
        {errorText && (
          <div
            role="alert"
            data-testid="log-viewer-error"
            style={{
              padding: '12px 16px',
              backgroundColor: 'rgba(239, 68, 68, 0.15)',
              borderBottom: '1px solid rgba(239, 68, 68, 0.3)',
              color: '#fca5a5',
              fontSize: '12px',
              display: 'flex',
              alignItems: 'center',
              gap: '8px',
            }}
          >
            <span>⚠️</span>
            <span>Error streaming logs: {errorText}</span>
          </div>
        )}

        {/* Loading Indicator */}
        {isBusy && rawLogs.length === 0 && (
          <div
            role="status"
            data-testid="log-viewer-loading"
            style={{
              padding: '40px 20px',
              textAlign: 'center',
              color: '#94a3b8',
              display: 'flex',
              flexDirection: 'column',
              alignItems: 'center',
              gap: '10px',
            }}
          >
            <div
              style={{
                width: '24px',
                height: '24px',
                border: '2px solid #334155',
                borderTopColor: '#38bdf8',
                borderRadius: '50%',
                animation: 'spin 1s linear infinite',
              }}
            />
            <span>Loading log stream…</span>
          </div>
        )}

        {/* Empty State Component: Rendered when zero matching results */}
        {!isBusy && !errorText && hasZeroResults && (
          <>
            {emptyState || (
              <LogEmptyState
                searchQuery={activeSearchQuery}
                onClearSearch={hasSearchFilter ? handleClearSearch : undefined}
                title={emptyStateTitle}
                description={emptyStateDescription}
              />
            )}
          </>
        )}

        {/* Log Entries List */}
        {!errorText && !hasZeroResults && (
          <div
            className="log-lines-list"
            data-testid="log-lines-list"
            style={{
              padding: '10px 0',
              lineHeight: 1.6,
            }}
          >
            {filteredEntries.map((entry, idx) => {
              const levelStyle = getLevelBadgeStyle(entry.level);
              return (
                <div
                  key={entry.id ?? idx}
                  className="log-line"
                  data-testid="log-line"
                  data-level={entry.level}
                  style={{
                    display: 'flex',
                    alignItems: 'flex-start',
                    padding: '2px 14px',
                    fontFamily: 'inherit',
                    whiteSpace: 'pre-wrap',
                    wordBreak: 'break-all',
                    transition: 'background-color 0.1s ease',
                  }}
                  onMouseEnter={(e) => {
                    e.currentTarget.style.backgroundColor = 'rgba(255, 255, 255, 0.03)';
                  }}
                  onMouseLeave={(e) => {
                    e.currentTarget.style.backgroundColor = 'transparent';
                  }}
                >
                  {showLineNumbers && (
                    <span
                      className="log-line-number"
                      aria-hidden="true"
                      style={{
                        userSelect: 'none',
                        color: '#475569',
                        width: '38px',
                        minWidth: '38px',
                        textAlign: 'right',
                        paddingRight: '12px',
                        fontSize: '11px',
                        lineHeight: 1.8,
                      }}
                    >
                      {idx + 1}
                    </span>
                  )}

                  {showTimestamps && entry.timestamp && (
                    <span
                      className="log-timestamp"
                      style={{
                        color: '#64748b',
                        marginRight: '10px',
                        fontSize: '11px',
                        lineHeight: 1.8,
                        userSelect: 'none',
                      }}
                    >
                      {typeof entry.timestamp === 'string'
                        ? entry.timestamp
                        : entry.timestamp instanceof Date
                        ? entry.timestamp.toISOString()
                        : String(entry.timestamp)}
                    </span>
                  )}

                  {entry.level && (
                    <span
                      className="log-level-badge"
                      style={{
                        color: levelStyle.color,
                        backgroundColor: levelStyle.bg,
                        border: `1px solid ${levelStyle.border}`,
                        borderRadius: '3px',
                        padding: '1px 5px',
                        fontSize: '10px',
                        fontWeight: 600,
                        textTransform: 'uppercase',
                        marginRight: '10px',
                        lineHeight: 1.5,
                        userSelect: 'none',
                      }}
                    >
                      {entry.level}
                    </span>
                  )}

                  {entry.source && (
                    <span
                      className="log-source"
                      style={{
                        color: '#94a3b8',
                        marginRight: '8px',
                        fontSize: '11px',
                        fontWeight: 500,
                      }}
                    >
                      [{entry.source}]
                    </span>
                  )}

                  <span
                    className="log-message"
                    style={{
                      flex: 1,
                      color:
                        entry.level === 'error' || entry.level === 'fatal'
                          ? '#fca5a5'
                          : entry.level === 'warn' || entry.level === 'warning'
                          ? '#fde68a'
                          : '#e2e8f0',
                    }}
                  >
                    {entry.message}
                  </span>
                </div>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
});

export default LogViewer;
