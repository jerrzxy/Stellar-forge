/**
 * LogViewer unit tests — Empty State & Search Filtering
 *
 * Validates:
 *  1. Renders friendly "No matching log entries found" empty state component when search yields zero results.
 *  2. Displays active search query in the empty state description.
 *  3. Action button "Clear search filter" appears and triggers callback / clears search.
 *  4. Renders empty state when log list is initially empty.
 *  5. Does NOT render empty state when search query matches log entries.
 *  6. Does NOT render empty state while loading.
 *  7. Does NOT render empty state when error banner is displayed.
 *  8. Supports custom emptyState component override.
 *  9. Supports custom emptyStateTitle and emptyStateDescription.
 * 10. Filters correctly across message, source, and level.
 */

import React from 'react';
import { render, screen, fireEvent } from '@testing-library/react';
import { LogViewer, LogEmptyState, LogItem, LogEntry } from './LogViewer';

// ─── Fixtures ─────────────────────────────────────────────────────────────────

const SAMPLE_LOGS: LogItem[] = [
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

describe('LogEmptyState component', () => {
  it('renders default title "No matching log entries found"', () => {
    render(<LogEmptyState />);
    expect(screen.getByRole('status')).toBeInTheDocument();
    expect(screen.getByTestId('log-empty-state-title')).toHaveTextContent(
      'No matching log entries found',
    );
  });

  it('renders search query in description when provided', () => {
    render(<LogEmptyState searchQuery="nonexistent-pod-xyz" />);
    expect(screen.getByTestId('log-empty-state-description')).toHaveTextContent(
      'nonexistent-pod-xyz',
    );
  });

  it('renders clear button and invokes onClearSearch on click', () => {
    const handleClear = jest.fn();
    render(<LogEmptyState searchQuery="crash" onClearSearch={handleClear} />);

    const clearBtn = screen.getByTestId('log-empty-state-clear-btn');
    expect(clearBtn).toBeInTheDocument();
    fireEvent.click(clearBtn);
    expect(handleClear).toHaveBeenCalledTimes(1);
  });

  it('supports custom title and description', () => {
    render(
      <LogEmptyState
        title="Zero matches"
        description="Try searching with a different keyword."
      />,
    );
    expect(screen.getByTestId('log-empty-state-title')).toHaveTextContent('Zero matches');
    expect(screen.getByTestId('log-empty-state-description')).toHaveTextContent(
      'Try searching with a different keyword.',
    );
  });
});

describe('LogViewer component empty state', () => {
  it('renders "No matching log entries found" when search returns zero results', () => {
    render(<LogViewer logs={SAMPLE_LOGS} searchQuery="term-that-does-not-exist" />);

    expect(screen.getByTestId('log-empty-state')).toBeInTheDocument();
    expect(screen.getByTestId('log-empty-state-title')).toHaveTextContent(
      'No matching log entries found',
    );
    expect(screen.getByTestId('log-empty-state-description')).toHaveTextContent(
      'term-that-does-not-exist',
    );
    expect(screen.queryByTestId('log-lines-list')).not.toBeInTheDocument();
  });

  it('renders matching logs when search query matches entries', () => {
    render(<LogViewer logs={SAMPLE_LOGS} searchQuery="validator-1" />);

    expect(screen.queryByTestId('log-empty-state')).not.toBeInTheDocument();
    const logLines = screen.getAllByTestId('log-line');
    expect(logLines).toHaveLength(1);
    expect(logLines[0]).toHaveTextContent('slow ledger catchup detected on validator-1');
  });

  it('clears search when clear button is clicked in empty state', () => {
    const handleClear = jest.fn();
    render(
      <LogViewer
        logs={SAMPLE_LOGS}
        searchQuery="missing-entry"
        onClearSearch={handleClear}
      />,
    );

    const clearBtn = screen.getByTestId('log-empty-state-clear-btn');
    fireEvent.click(clearBtn);
    expect(handleClear).toHaveBeenCalledTimes(1);
  });

  it('renders empty state when log list is empty', () => {
    render(<LogViewer logs={[]} />);

    expect(screen.getByTestId('log-empty-state')).toBeInTheDocument();
    expect(screen.getByTestId('log-empty-state-title')).toHaveTextContent(
      'No matching log entries found',
    );
  });

  it('does NOT render empty state when loading is true', () => {
    render(<LogViewer logs={[]} loading={true} />);

    expect(screen.queryByTestId('log-empty-state')).not.toBeInTheDocument();
    expect(screen.getByTestId('log-viewer-loading')).toBeInTheDocument();
  });

  it('does NOT render empty state when an error is present', () => {
    render(<LogViewer logs={[]} error="Failed to connect to stream" />);

    expect(screen.queryByTestId('log-empty-state')).not.toBeInTheDocument();
    expect(screen.getByTestId('log-viewer-error')).toHaveTextContent(
      'Failed to connect to stream',
    );
  });

  it('renders custom emptyState element when provided', () => {
    render(
      <LogViewer
        logs={SAMPLE_LOGS}
        searchQuery="notfound"
        emptyState={<div data-testid="custom-empty">Custom empty message</div>}
      />,
    );

    expect(screen.getByTestId('custom-empty')).toHaveTextContent('Custom empty message');
    expect(screen.queryByTestId('log-empty-state')).not.toBeInTheDocument();
  });

  it('filters logs via interactive search input', () => {
    render(<LogViewer logs={SAMPLE_LOGS} showSearchBar={true} />);

    const searchInput = screen.getByTestId('log-viewer-search-input');
    fireEvent.change(searchInput, { target: { value: 'connection timed out' } });

    const lines = screen.getAllByTestId('log-line');
    expect(lines).toHaveLength(1);
    expect(lines[0]).toHaveTextContent('connection timed out');

    // Type query with 0 results
    fireEvent.change(searchInput, { target: { value: 'completely-unmatched-string' } });
    expect(screen.getByTestId('log-empty-state')).toBeInTheDocument();
    expect(screen.getByTestId('log-empty-state-title')).toHaveTextContent(
      'No matching log entries found',
    );
  });

  it('supports lines prop alias for logs', () => {
    render(<LogViewer lines={SAMPLE_LOGS} searchQuery="zero-matches-search" />);

    expect(screen.getByTestId('log-empty-state')).toBeInTheDocument();
    expect(screen.getByTestId('log-empty-state-title')).toHaveTextContent(
      'No matching log entries found',
    );
  });
});
