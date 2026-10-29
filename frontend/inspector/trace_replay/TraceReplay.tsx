import React, { useEffect, useState, useRef } from 'react';
import { parseTraceForReplay, TraceReplayData } from './trace_parser';
import { TimelineScrubber } from '../timeline_scrubber';
import { TraceControls } from '../timeline_scrubber';

/**
 * Main Trace Replay component that provides a step-by-step debugging
 * experience for failed or complex smart contract calls.
 *
 * Features:
 * - Step forward/backward through WASM execution steps
 * - Live state of contract storage, CPU instruction count, and memory
 * - Play/pause automatic playback
 * - Instant scrubbing between steps
 */
export const TraceReplay: React.FC<{
  /** Raw trace data from Soroban RPC or flamegraph format */
  rawTraceData: any;
  /** Optional Soroban RPC URL for fetching additional context */
  rpcUrl?: string;
  /** Optional default RPC URL if none provided */
  defaultRpcUrl?: string;
}> = ({ rawTraceData, rpcUrl, defaultRpcUrl }) => {
  const [traceData, setTraceData] = useState<TraceReplayData | null>(null);
  const [isLoaded, setIsLoaded] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // Parse the trace data when rawTraceData changes
  useEffect(() => {
    try {
      setError(null);
      const parsed = parseTraceForReplay(rawTraceData);
      setTraceData(parsed);
      setIsLoaded(true);
    } catch (err: any) {
      setError(err.message || 'Failed to parse trace data');
      setIsLoaded(true);
    }
  }, [rawTraceData]);

  // If we have trace data but it's not loaded yet, show loading state
  if (!isLoaded && !error) {
    return (
      <div className="h-64 bg-slate-900 border border-slate-700 rounded-xl flex items-center justify-center">
        <span className="text-slate-400">Loading trace...</span>
      </div>
    );
  }

  // If there's an error, show error state
  if (error) {
    return (
      <div className="bg-red-900/30 border border-red-800 text-red-300 p-6 rounded-xl">
        <h3 className="text-red-400 mb-3">Trace Parse Error</h3>
        <p className="text-slate-300">{error}</p>
        <p className="text-slate-500 text-sm">Ensure the trace data is from a valid Soroban transaction.</p>
      </div>
    );
  }

  // If no trace data available
  if (!traceData || traceData.steps.length === 0) {
    return (
      <div className="bg-slate-900 border border-slate-700 rounded-xl p-6 text-center">
        <h3 className="text-slate-400 mb-3">No Trace Data</h3>
        <p className="text-slate-500">
          No execution trace found. This may be a successful transaction
          or a transaction without diagnostic information.
        </p>
      </div>
    );
  }

  const totalSteps = traceData.steps.length;
  const [currentStepIndex, setCurrentStepIndex] = useState(0);
  const [isPlaying, setIsPlaying] = useState(false);
  const playbackRef = useRef<number | null>(null);

  // Handle step change - update playback state
  const handleStepChange = useCallback((step: typeof traceData.steps[0]) => {
    setCurrentStepIndex(prev => {
      const index = traceData.steps.indexOf(step);
      return index >= 0 ? index : 0;
    });
  }, [traceData]);

  // Start auto-play when transitioning from paused to playing
  useEffect(() => {
    if (isPlaying && totalSteps > 1) {
      let animationId: number;
      const stepDuration = 500; // ms per step

      let currentIdx = currentStepIndex;

      const tick = () => {
        if (currentIdx >= totalSteps - 1) {
          setIsPlaying(false);
          return;
        }
        currentIdx++;
        setCurrentStepIndex(currentIdx);
        animationId = requestAnimationFrame(tick);
      };

      tick();
      playbackRef.current = animationId;
    } else if (!isPlaying && playbackRef.current !== null) {
      cancelAnimationFrame(playbackRef.current);
      playbackRef.current = null;
    }

    return () => {
      if (playbackRef.current !== null) {
        cancelAnimationFrame(playbackRef.current);
      }
    };
  }, [isPlaying, currentStepIndex, totalSteps]);

  // Cleanup on unmount
  useEffect(() => {
    return () => {
      if (playbackRef.current !== null) {
        cancelAnimationFrame(playbackRef.current);
      }
    };
  }, []);

  return (
    <div className="bg-slate-900 border border-slate-700 rounded-xl overflow-hidden shadow-lg">
      {/* Header */}
      <div className="p-6 border-b border-slate-700">
        <div className="flex items-center justify-between">
          <div className="flex items-center gap-3">
            <span className="text-slate-200 font-bold text-lg">
              {traceData.description || 'Trace Replay'}
            </span>
            <span className="text-slate-500 text-xs">
              {totalSteps} steps · {traceData.txHash?.slice(0, 8)}...
            </span>
          </div>

          <div className="flex items-center gap-2">
            <span className="text-slate-400 text-sm font-mono">
              Step {currentStepIndex + 1} of {totalSteps}
            </span>
          </div>
        </div>
      </div>

      {/* Controls */}
      <div className="p-4 border-b border-slate-700 flex flex-col sm:flex-row gap-3">
        <TraceControls
          traceData={traceData}
          onStepChange={handleStepChange}
          currentStepIndex={currentStepIndex}
          setCurrentStepIndex={setCurrentStepIndex}
          isPlaying={isPlaying}
          setIsPlaying={setIsPlaying}
        />
      </div>

      {/* Main content - timeline scrubber + step details */}
      <div className="p-6 flex flex-col sm:flex-row gap-6">
        {/* Left: Timeline Scrubber with media player controls */}
        <TimelineScrubber
          traceData={traceData}
          onStepChange={handleStepChange}
        />

        {/* Right: Live state display */}
        <div className="w-full sm:w-80 bg-slate-800 border border-slate-700 rounded-xl p-6">
          {currentStepIndex >= 0 && currentStepIndex < totalSteps && (
            <div>
              <h3 className="text-slate-200 font-medium mb-4">
                Live State at Step {currentStepIndex + 1}
              </h3>

              {/* CPU Instruction Count */}
              <div className="mb-4 p-3 bg-slate-900 rounded border border-slate-700">
                <span className="text-slate-400 text-sm font-medium">CPU Instructions</span>
                <div className="mt-2 flex items-baseline gap-2">
                  <span className="text-slate-300 font-mono text-lg">
                    {traceData.steps[currentStepIndex].instructionCount}
                  </span>
                  <span className="text-slate-500 text-xs">instructions</span>
                </div>
                {/* Simple visualization - more instructions = darker */}
                <div className="mt-3 h-2 bg-blue-500 rounded-w-full overflow-hidden" style={{ width: `${Math.min(
                  100,
                  (traceData.steps[currentStepIndex].instructionCount / 
                  Math.max(...traceData.steps.map(s => s.instructionCount)) || 1) * 100
                }%` }} />
              </div>

              {/* Memory Allocation */}
              <div className="mb-4 p-3 bg-slate-900 rounded border border-slate-700">
                <span className="text-slate-400 text-sm font-medium">Memory Allocation</span>
                <div className="mt-2 flex items-baseline gap-2">
                  <span className="text-slate-300 font-mono text-lg">
                    {traceData.steps[currentStepIndex].memoryAllocation}
                  </span>
                  <span className="text-slate-500 text-xs">bytes</span>
                </div>
              </div>

              {/* Contract Storage */}
              <div>
                <span className="text-slate-400 text-sm font-medium">Storage Slots</span>
                <div className="mt-2">
                  <span className="text-slate-300 font-mono text-sm">
                    {Object.keys(
                      traceData.steps[currentStepIndex].storage
                    ).length}
                  </span>
                  slots
                </div>
                {/* Storage preview - truncated */}
                <div className="mt-3 h-40 overflow-auto bg-slate-950 rounded">
                  <pre className="text-xs text-slate-500 whitespace-pre-wrap">
{JSON.stringify(
                    traceData.steps[currentStepIndex].storage,
                    null,
                    2
                  ).substring(0, 400)}{Object.keys(
                    traceData.steps[currentStepIndex].storage
                  ).length > 5 ? '...' : ''}
                  </pre>
                </div>
              </div>
            </div>
          )}
        </div>
      </div>
    </div>
  );
};

/** Preview a single step's storage changes. */
export const StoragePreview = ({
  storage,
  stepIndex,
  totalSteps,
}: {
  storage: Record<string, string>;
  stepIndex: number;
  totalSteps: number;
}) => {
  return (
    <div className="bg-slate-900 rounded border border-slate-700 p-4 max-h-64 overflow-auto">
      <div className="flex items-center justify-between mb-3">
        <span className="text-slate-300 text-sm">Step {stepIndex + 1}/{totalSteps}</span>
        <span className="text-slate-500 text-xs">{Object.keys(storage).length} slots</span>
      </div>
      <pre className="text-xs text-slate-500 whitespace-pre-wrap">
{JSON.stringify(storage, null, 2).substring(0, 300)}{Object.keys(storage).length > 5 ? ' ...' : ''}
      </pre>
    </div>
  };
};