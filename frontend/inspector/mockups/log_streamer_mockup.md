# Real-Time Validator Log Streamer Mockup

## Overview
A fluid, color-coded, and filterable log inspection dashboard for stellar-core consensus events from the operator.

## Layout

```
+-------------------------------------------------------+
| Log Streamer                          Connected     |
+-------------------------------------------------------+
| ┌───────────────────────────────────────────────────┐ |
| │  ←  State Fell Behind   Lost Sync   Catchup Started │ |
| │  (highlighted in red)          (highlighted in     │ |
| │   orange)                       (highlighted in    │ |
| │   green)                                                │ |
| └───────────────────────────────────────────────────┘ |
|-------------------------------------------------------|
|  ────  ────  ────  ●  ────  ────  ────  ────  ────   │
|     timestamps               events per second         │
+-------------------------------------------------------+
|  [ Search: State Fell Behind]  [Clear]               |
|-------------------------------------------------------|
|  ────  ────  ────  ────  ────  ────  ────  ────  ────   │
|  14:23:45 ERROR State Fell Behind at ledger 5012     │
|  14:23:44 WARN  Memory pressure at node-3             │
|  14:23:43 INFO  Starting up validator                │
|  14:23:42 ERROR Lost Sync detected                   │
|  14:23:41 INFO  Sync in progress                     │
|  14:23:40 ERROR Catchup Started at ledger 4990        │
|  14:23:39 INFO  Validator key rotation               │
|  ────  ────  ────  ────  ────  ────  ────  ────  ────   │
|  Total: 1,234 lines | Errors: 12 | Warnings: 5       │
+-------------------------------------------------------+
```

## Features

### Pattern Highlighting (Default Rules)
| Pattern                | Regex                      | Color/Highlight     |
|------------------------|----------------------------|---------------------|
| State Fell Behind      | `/State Fell Behind/`      | bg-red-500/20       |
| Lost Sync              | `/Lost Sync/`              | bg-orange-500/20    |
| Catchup Started        | `/Catchup Started/`        | bg-green-500/20     |
| Disk Usage             | `/disk usage/i`            | bg-amber-500/20     |
| Memory Pressure        | `/memory pressure/i`       | bg-amber-500/20     |
| Info messages          | `/info/i`                  | bg-blue-500/20      |

### Virtualized List
- Renders thousands of log lines without browser lag
- Fixed row height (28px) with overscan (6 rows above/below viewport)
- Only visible + overscanned rows in DOM
- Flat memory consumption over hours of streaming

### Search/Filter
- Live search input highlighting matching lines
- Toggle pattern rules on/off
- Clear filter button

### Connection Status
- Connected/Reconnecting/Offline indicators
- Events per second metric
- Total lines received

## Interaction Flow

1. User navigates to operator dashboard log streamer
2. WebSocket connection establishes to stellar-operator logging proxy
3. Logs stream in real-time with automatic pattern highlighting
4. User can:
   - Search/filter by consensus events
   - Toggle pattern rules on/off
   - Click any log line for detailed inspection
   - Clear filter to see all logs
5. Memory usage remains stable over time (virtualized rendering)

## Performance Requirements
- **Memory**: Flat consumption regardless of stream duration
- **Rendering**: 60fps even with 1000+ log lines/sec
- **Scrubbing**: Instant timeline navigation (no reload)
- **Pattern matching**: Regex test on each incoming line (< 1ms overhead)