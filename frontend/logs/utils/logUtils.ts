/**
 * Log viewer utility functions for filtering, normalization, and empty-state calculation.
 */

export interface NormalizedLogEntry {
  id: string | number;
  message: string;
  level: string;
  timestamp?: string | number | Date;
  source?: string;
  metadata?: Record<string, unknown>;
}

export type InputLogItem = string | Partial<NormalizedLogEntry>;

export function normalizeLog(item: InputLogItem, index: number): NormalizedLogEntry {
  if (typeof item === 'string') {
    let level = 'info';
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

export function filterLogs(
  logs: InputLogItem[],
  query?: string,
  levelFilter?: string,
): NormalizedLogEntry[] {
  const normalized = logs.map((l, i) => normalizeLog(l, i));

  return normalized.filter((entry) => {
    // Level filter
    if (levelFilter && levelFilter !== 'all') {
      if ((entry.level || '').toLowerCase() !== levelFilter.toLowerCase()) {
        return false;
      }
    }

    // Search query filter
    if (query && query.trim()) {
      const q = query.trim().toLowerCase();
      const msg = entry.message.toLowerCase();
      const src = (entry.source || '').toLowerCase();
      const lvl = (entry.level || '').toLowerCase();
      if (!msg.includes(q) && !src.includes(q) && !lvl.includes(q)) {
        return false;
      }
    }

    return true;
  });
}

export interface EmptyStateEvaluation {
  shouldShowEmptyState: boolean;
  title: string;
  description: string;
  hasActiveSearch: boolean;
}

export function evaluateEmptyState(params: {
  rawLogs: InputLogItem[];
  filteredLogs: NormalizedLogEntry[];
  searchQuery?: string;
  levelFilter?: string;
  loading?: boolean;
  error?: unknown;
  customTitle?: string;
  customDescription?: string;
}): EmptyStateEvaluation {
  const {
    filteredLogs,
    searchQuery = '',
    levelFilter = 'all',
    loading = false,
    error = null,
    customTitle,
    customDescription,
  } = params;

  if (loading || error) {
    return {
      shouldShowEmptyState: false,
      title: '',
      description: '',
      hasActiveSearch: false,
    };
  }

  const hasSearch = Boolean(searchQuery.trim() || (levelFilter && levelFilter !== 'all'));
  const hasZeroResults = filteredLogs.length === 0;

  if (!hasZeroResults) {
    return {
      shouldShowEmptyState: false,
      title: '',
      description: '',
      hasActiveSearch: hasSearch,
    };
  }

  const title = customTitle || 'No matching log entries found';
  let description = customDescription;
  if (!description) {
    if (searchQuery.trim()) {
      description = `No log lines match your query "${searchQuery.trim()}". Try revising your terms or resetting active filters.`;
    } else {
      description = 'No matching log entries were found matching your current filter criteria. Check your query syntax or widen your search scope.';
    }
  }

  return {
    shouldShowEmptyState: true,
    title,
    description,
    hasActiveSearch: hasSearch,
  };
}
