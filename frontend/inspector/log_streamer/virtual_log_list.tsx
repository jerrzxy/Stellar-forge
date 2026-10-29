import React from 'react';
import { FixedSizeList as VirtualLogList } from 'react-window';
import type { LogLine, LogPatternRule } from './types';

/** Row renderer for react-window. Renders a single log line with pattern highlighting. */
const LogRow = ({ index, style, data }: { index: number; style: React.CSSProperties; data: LogLine }) => {
  const line = data;
  // Determine if this line matches any pattern
  const matchingRule = line.patternRules?.find((rule) => rule.regex.test(line.message));
  const hasHighlight = !!matchingRule;
  const highlightClass = matchingRule?.highlightClass ?? 'bg-emerald-500/20';

  // Format the display message - truncate if too long
  const displayMessage = line.message.length > 150
    ? `${line.message.substring(0, 150)}…`
    : line.message;

  const levelClass = `badge badge--${line.level}`;

  return (
    <div
      role="row"
      style={style}
      className={
        `log-row log-row--${line.level}${hasHighlight ? ' has-highlight' : ''}`
      }
    >
      {/* Timestamp */}
      <span className="log-cell log-cell--timestamp" style={{ width: '120px' }}>
        {new Date(line.timestamp).toLocaleTimeString()}
      </span>

      {/* Level badge */}
      <span className={levelClass} style={{ width: '70px' }}>
        {line.level.toUpperCase()}
      </span>

      {/* Message */}
      <span className="log-cell log-cell--message" style={{ flex: '1', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
        {displayMessage}
      </span>
    </div>
  );
};

/**
 * Virtualized log list using react-window for rendering thousands of log lines.
 * Fixed row height of 28px with 6-row overscan above and below the viewport.
 * Provides color-coded highlights for critical consensus events.
 */
export function VirtualLogList({ 
  logLines, 
  patternRules, 
  onLineClick,
  autoScroll 
}: VirtualLogListProps) {
  const containerRef = useRef<HTMLDivElement>(null!);

  return (
    <div className="bg-slate-900 border border-slate-700 rounded-lg overflow-hidden">
      {/* Column headers */}
      <div className="p-2 border-b border-slate-700 flex items-center text-xs text-slate-400">
        <span className="flex-1">Timestamp</span>
        <span className="w-20 text-center">Level</span>
        <span className="flex-1">Message</span>
      </div>

      {/* Scrollable virtual list using react-window */}
      <FixedSizeList
        width="100%"
        height={400}
        itemCount={logLines.length}
        itemSize={28}
        overscan={6}
        className="event-scroll log-scroll"
        role="grid"
        aria-rowcount={logLines.length}
        aria-label="Stellar-core logs"
        style={{ overflow: 'auto' }}
      {
        ({ index, style, data }) => (
          <LogRow
            index={index}
            style={style}
            data={data as LogLine}
            patternRules={patternRules as LogPatternRule[]}
            onClick={() => onLineClick?.(data as LogLine)}
          />
        )
      }/>
    </div>
  );
}
;