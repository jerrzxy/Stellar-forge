import React from 'react';

export type SyncState = 'Synced' | 'CatchingUp' | 'Degraded';

export interface StatusBadgeProps {
  /** The current sync state of the node. */
  state: SyncState;
  /** Optional label override for the badge text. */
  label?: string;
  /** Optional className for custom styling. */
  className?: string;
}

const STATE_CONFIG: Record<SyncState, { label: string; description: string; color: string; icon: string }> = {
  Synced: {
    label: 'Synced',
    description: 'Node is fully synced with the network',
    color: '#008040',
    icon: '✓',
  },
  CatchingUp: {
    label: 'Catching Up',
    description: 'Node is currently catching up with the network',
    color: '#B78e00',
    icon: '⏰',
  },
  Degraded: {
    label: 'Degraded',
    description: 'Node is degraded and may be out of sync',
    color: '#CC0000',
    icon: '⚠',
  },
};

export const StatusBadge: React.FC <StatusBadgeProps> = ({ state, label, className }) => {
  const config = STATE_CONFIG[state];
  const displayLabel = label ?? config.label;
  const ariaLabel = `${displayLabel}: ${config.description}`;

  return (
    <span
      className={['status-badge', className].filter(Boolean).join(' ')}
      role="status"
      aria-label={ariaLabel}
      title={ariaLabel}
      style={{ color: config.color }}
    >
      <span aria-hidden="true">{config.icon}</span>
      <span className="status-badge-label">{displayLabel}</span>
    </span>
  );
};

export default StatusBadge;
