/**
 * Client for disaster-recovery drill APIs.
 */

export type DrPhaseKey =
  | 'snapshot_restoration'
  | 'pod_recreation'
  | 'catchup_sync'
  | 'traffic_redirection';

export type DrPhaseStatus =
  | 'pending'
  | 'running'
  | 'passed'
  | 'failed'
  | 'skipped';

export interface DrPhaseState {
  status: DrPhaseStatus;
  message?: string;
  started_at?: string;
  completed_at?: string;
}

export interface DrTriggerResponse {
  drill_id: string;
  node: string;
  dry_run: boolean;
  queued_at?: string;
}

export interface DrStatusResponse {
  drill_id: string;
  node: string;
  status?: string;
  phases?: Partial<Record<DrPhaseKey, DrPhaseState>>;
}

export interface DrResetResponse {
  node: string;
  reset_at?: string;
}

interface DrRequestOptions {
  apiBase?: string;
}

async function request<T>(url: string, init?: RequestInit): Promise<T> {
  const response = await fetch(url, init);
  if (!response.ok) {
    const operation = url.includes('/trigger')
      ? 'DR trigger'
      : url.includes('/reset') ? 'DR reset' : 'DR status';
    const detail = typeof response.text === 'function' ? await response.text() : '';
    throw new Error(
      `${operation} failed: ${response.status} ${response.statusText}${detail ? ` — ${detail}` : ''}`,
    );
  }
  return response.json() as Promise<T>;
}

const drClient = {
  trigger(
    node: string,
    { apiBase = '', dryRun = true }: DrRequestOptions & { dryRun?: boolean } = {},
  ): Promise<DrTriggerResponse> {
    return request<DrTriggerResponse>(`${apiBase}/api/dr/trigger`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ node, dry_run: dryRun }),
    });
  },

  status(drillId: string, { apiBase = '' }: DrRequestOptions = {}): Promise<DrStatusResponse> {
    return request<DrStatusResponse>(
      `${apiBase}/api/dr/status/${encodeURIComponent(drillId)}`,
    );
  },

  reset(node: string, { apiBase = '' }: DrRequestOptions = {}): Promise<DrResetResponse> {
    return request<DrResetResponse>(`${apiBase}/api/dr/reset`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ node }),
    });
  },

  wsUrl(wsBase = 'ws://localhost:8080'): string {
    return `${wsBase.replace(/\/$/, '')}/api/dr/stream`;
  },
};

export default drClient;
