import React, { useState, useRef, useCallback } from 'react';
import { TraceStep, TraceReplayData } from './trace_replay/types';

/** Controls for the trace timeline scrubber (media player style). */
export const TimelineScrubber: React.FC<{
  /** The trace data to replay. */
  traceData: TraceReplayData;
  /** Callback when step changes. */
  onStepChange: (step: TraceStep) => void;
  /** Optional callback when playback starts. */
  onPlaybackStart?: () => void;
  /** Optional callback when playback stops. */
  onPlaybackStop?: () => void;
}> = ({ traceData, onStepChange, onPlaybackStart, onPlaybackStop }) => {
  const [currentStepIndex, setCurrentStepIndex] = useState(0);
  const [isPlaying, setIsPlaying] = useState(false);
  const [isScrubbing, setIsScrubbing] = useState(false);
  const steps = traceData.steps;

  const totalSteps = steps.length;
  const currentStep = steps[currentStepIndex];

  // Seek to a specific step index
  const seekStep = useCallback((index: number) => {
    const clampedIndex = Math.max(0, Math.min(totalSteps - 1, index));
    if (clampedIndex !== currentStepIndex) {
      setCurrentStepIndex(clampedIndex);
      onStepChange?.(steps[clampedIndex]);
    }
  }, [currentStepIndex, totalSteps, onStepChange, steps]);

  // Playback control functions
  const handlePlayPause = useCallback(() => {
    setIsPlaying(prev => !prev);
    if (prev) {
      onPlaybackStop?.();
    } else {
      onPlaybackStart?.();
    }
  }, [onPlaybackStart, onPlaybackStop]);

  const stepForward = useCallback(() => {
    if (isScrubbing) return;
    setIsPlaying(true);
    seekStep(currentStepIndex + 1);
  }, [isScrubbing, seekStep, currentStepIndex]);

  const stepBack = useCallback(() => {
    if (isScrubbing) return;
    setIsPlaying(true);
    seekStep(currentStepIndex - 1);
  }, [isScrubbing, seekStep, currentStepIndex]);

  // Hide controls when scrubbing
  const showControls = !isScrubbing;

  return (
    <div className="bg-slate-900 border border-slate-700 rounded-xl p-6 shadow-lg">
      <div className="flex items-center justify-between mb-4">
        <div className="flex items-center gap-3">
          <span className="text-slate-400 text-sm font-mono">
            {currentStepIndex + 1}/{totalSteps}
          </span>
          <span className="text-slate-200 text-sm font-medium">
            {currentStep?.eventName || 'Step'}
          </span>
        </div>

        <div className="flex gap-2">
          {isPlaying ? (
            <button
              onClick={handlePlayPause}
              title="Pause"
              className="bg-blue-600 hover:bg-blue-700 text-white font-medium py-1 px-3 rounded"
            >
              ⏸ Pause
            </button>
          ) : (
            <button
              onClick={handlePlayPause}
              title="Play"
              className="bg-emerald-600 hover:bg-emerald-700 text-white font-medium py-1 px-3 rounded"
            >
              ▶ Play
            </button>
          )}

          <button
            onClick={stepBack}
            disabled={currentStepIndex <= 0}
            title="Step Back"
            className="bg-slate-700 hover:bg-slate-600 text-slate-300 font-medium py-1 px-2 rounded"
          >
            ⏪
          </button>

          <button
            onClick={stepForward}
            disabled={currentStepIndex >= totalSteps - 1}
            title="Step Forward"
            className="bg-slate-700 hover:bg-slate-600 text-slate-300 font-medium py-1 px-2 rounded"
          >
            ⏩
          </button>
        </div>
      </div>

      {/* Timeline progress bar */}
      <div className="h-2 bg-slate-800 rounded-full overflow-hidden mb-4">
        <div
          className={`h-full bg-gradient-to-r from-blue-500 to-emerald-500 transition-width ease-out duration-300 ${
            isScrubbing ? 'bg-yellow-500' : ''
          }`}
          style={{
            width: `${(currentStepIndex / (totalSteps - 1)) * 100}%`,
          }}
        />
      </div>

      {/* Step detail display */}
      {currentStep && (
        <div className="bg-slate-800 p-4 rounded border border-slate-700">
          <h3 className="text-slate-200 font-medium mb-3">
            Step {currentStepIndex + 1}: {currentStep.eventName}
          </h3>

          <div className="grid grid-cols-2 gap-4 text-slate-400 text-sm">
            <div>
              <span className="font-semibold text-slate-200">CPU Instructions:</span>
              <span className="font-mono text-slate-300">{currentStep.instructionCount}</span>
            </div>
            <div>
              <span className="font-semibold text-slate-200">Storage Slots:</span>
              <span className="font-mono text-slate-300">
                {Object.keys(currentStep.storage).length}
              </span>
            </div>
            <div>
              <span className="font-semibold text-slate-200">Memory:</span>
              <span className="font-mono text-slate-300">{currentStep.memoryAllocation}</span>
            </div>
            <div>
              <span className="font-semibold text-slate-200">Ledger:</span>
              <span className="font-mono text-slate-300">{currentStep.ledgerSeq}</span>
            </div>
          </div>

          {/* Storage change preview */}
          {Object.keys(currentStep.storage).length > 0 && (
            <div className="mt-3 pt-3 border-t border-slate-700">
              <span className="text-slate-500 text-xs">Storage state:</span>
              <div className="mt-1 overflow-x-auto">
                <pre className="text-xs text-slate-500 overflow-auto">
{JSON.stringify(currentStep.storage, null, 2).substring(0, 500)}{Object.keys(currentStep.storage).length > 10 ? '...' : ''}
                </pre>
              </div>
            </div>
          )}
        </div>
      )}
    </div>
  );
};

/** Controls for navigating the trace timeline. */
export const TraceControls: React.FC<{
  /** The trace data to control. */
  traceData: TraceReplayData;
  /** Callback when step changes. */
  onStepChange: (step: TraceStep) => void;
  /** Current step index (controlled state). */
  currentStepIndex: number;
  /** Set current step index. */
  setCurrentStepIndex: (index: number) => void;
  /** Whether playback is in progress. */
  isPlaying: boolean;
  /** Set playing state. */
  setIsPlaying: (playing: boolean) => void;
}> = ({
  traceData,
  onStepChange,
  currentStepIndex,
  setCurrentStepIndex,
  isPlaying,
  setIsPlaying,
}) => {
  const totalSteps = traceData.steps.length;

  return (
    <div className="flex gap-2 flex-wrap">
      <button
        onClick={() => setCurrentStepIndex(Math.max(0, currentStepIndex - 1))}
        disabled={currentStepIndex <= 0}
        title="Previous Step"
        className="flex-1 bg-slate-700 hover:bg-slate-600 text-slate-300 py-2 px-3 rounded text-sm"
      >
        ⏪ Prev
      </button>

      <button
        onClick={() => setCurrentStepIndex(Math.min(totalSteps - 1, currentStepIndex + 1))}
        disabled={currentStepIndex >= totalSteps - 1}
        title="Next Step"
        className="flex-1 bg-slate-700 hover:bg-slate-600 text-slate-300 py-2 px-3 rounded text-sm"
      >
        Next ⏩
      </button>

      {/* Playback controls */}
      {totalSteps > 1 && (
        <div className="flex gap-2 mt-2">
          <button
            onClick={() => setIsPlaying(true)}
            title="Auto-play"
            className={`flex-1 ${
              isPlaying ? 'bg-blue-600 hover:bg-blue-700' : 'bg-slate-700 hover:bg-slate-600'
            } text-white py-2 px-3 rounded text-sm`}
          >
            ▶ Auto
          </button>

          <button
            onClick={() => setIsPlaying(false)}
            title="Stop"
            className={`flex-1 ${
              !isPlaying ? 'bg-emerald-600 hover:bg-emerald-700' : 'bg-slate-700 hover:bg-slate-600'
            } text-white py-2 px-3 rounded text-sm`}
          >
            ⏹ Stop
          </button>
        </div>
      )}
    </div>
  );
};