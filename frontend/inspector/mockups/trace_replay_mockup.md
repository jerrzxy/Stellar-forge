# Trace Replay UI Mockup

## Overview
A "media player" style interface for stepping through Soroban transaction execution traces to debug failed or complex smart contract calls.

## Layout

```
+-------------------------------------------------------+
| Trace Replay: 00123abc... | Step 3 of 45            |
+-------------------------------------------------------+
| ▶⏸ Pause   ⏪   ⏩                                    |
|-------------------------------------------------------|
| Live State at Step 3                                  |
|  CPU Instructions: 45,231  [██████████████░░░░░░]  |
|  Memory Allocation: 2,048 bytes                       |
|  Storage Slots: 12                                    |
|-------------------------------------------------------|
| Storage State (truncated):                            |
| { "key1": "value1", "key2": "value2", ... }           |
+-------------------------------------------------------+
```

## Controls

| Control        | Action                      |
|----------------|-----------------------------|
| ▶ Play         | Start automatic step-forward|
| ⏸ Pause        | Stop automatic playback     |
| ⏪ Step Back    | Go to previous step         |
| ⏩ Step Forward | Go to next step             |
| Progress bar   | Visual position in timeline |

## Live State Display

### CPU Instruction Count
- Large number showing total instructions at this step
- Progress bar visualization (relative to max across all steps)
- Darker color = more instructions

### Memory Allocation
- Byte count at this step
- Useful for detecting memory leaks

### Contract Storage
- Key-value pairs shown in truncated JSON
- Number of active storage slots
- Expandable/collapsible detail view

## Interaction Flow

1. User loads a Soroban transaction trace (RPC diagnostic events or flamegraph format)
2. Trace is parsed into sequential steps
3. User can:
   - Play automatically through all steps
   - Manually step forward/backward
   - Pause at any step to inspect live state
4. At each step, live state displays:
   - CPU instructions, memory, storage
   - Progress bar shows relative position

## Performance Notes
- State delta calculations between steps must be efficient
- Scrubbing timeline must be instant (no loading delay)
- Virtual scrolling not needed (typically < 1000 steps per trace)