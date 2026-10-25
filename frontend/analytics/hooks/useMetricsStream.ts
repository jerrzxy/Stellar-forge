import { useEffect, useRef, useState } from 'react';

export interface MetricsPoint {
  timestamp: number;
  value: number;
}

export interface MetricsStreamState {
  connected: boolean;
  points: MetricsPoint[];
  error: string | null;
}

const INITIAL_RECONNECT_DELAY_MS = 1000;
const MAX_RECONNECT_DELAY_MS = 30000;

export function useMetricsStream(url: string): MetricsStreamState {
  const [connected, setConnected] = useState(false);
  const [points, setPoints] = useState<MetricsPoint[]>([]);
  const [error, setError] = useState<string | null>(null);

  const socketRef = useRef<WebSocket | null>(null);
  const reconnectDelayRef = useRef<number>(INITIAL_RECONNECT_DELAY_MS);
  const reconnectTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const shouldReconnectRef = useRef<boolean>(true);

  useEffect(() => {
    shouldReconnectRef.current = true;

    const clearReconnectTimer = () => {
      if (reconnectTimerRef.current !== null) {
        clearTimeout(reconnectTimerRef.current);
        reconnectTimerRef.current = null;
      }
    };

    const scheduleReconnect = () => {
      if (!shouldReconnectRef.current) {
        return;
      }

      clearReconnectTimer();

      const delay = reconnectDelayRef.current;
      reconnectTimerRef.current = setTimeout(() => {
        reconnectTimerRef.current = null;
        connect();
      }, delay);

      reconnectDelayRef.current = Math.min(
        reconnectDelayRef.current * 2,
        MAX_RECONNECT_DELAY_MS,
      );
    };

    const connect = () => {
      if (!shouldReconnectRef.current) {
        return;
      }

      try {
        const socket = new WebSocket(url);
        socketRef.current = socket;

        socket.onopen = () => {
          setConnected(true);
          setError(null);
          reconnectDelayRef.current = INITIAL_RECONNECT_DELAY_MS;
        };

        socket.onmessage = (event: MessageEvent) => {
          try {
            const parsed = JSON.parse(event.data as string) as MetricsPoint;
            setPoints((prev) => [...prev, parsed]);
          } catch {
            setError('Failed to parse metrics message');
          }
        };

        socket.onerror = () => {
          setError('WebSocket error');
        };

        socket.onclose = () => {
          setConnected(false);
          socketRef.current = null;
          scheduleReconnect();
        };
      } catch {
        setError('Failed to establish WebSocket connection');
        scheduleReconnect();
      }
    };

    connect();

    return () => {
      shouldReconnectRef.current = false;
      clearReconnectTimer();

      if (socketRef.current) {
        socketRef.current.onopen = null;
        socketRef.current.onmessage = null;
        socketRef.current.onerror = null;
        socketRef.current.onclose = null;
        socketRef.current.close();
        socketRef.current = null;
      }
    };
  }, [url]);

  return { connected, points, error };
}