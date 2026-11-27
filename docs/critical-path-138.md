# Build 0.138 — critical-path capture, not a performance patch

No rendering, synchronization, residency or optimizer defaults were changed. The previous internal timeline already records wrapped CPU waits/wakes, submission stages, relay and transfer spans, and calibrated direct-queue timestamps. This build adds recording IDs, queue/translation milestones, gap partitioning, timestamp coverage and process fault/working-set snapshots. The heuristic wake chain is not proof of causality. GPU gaps cover instrumented direct submissions only; copy/present work may overlap them. The process page-fault count includes soft faults; hard faults, scheduler ready/running states and disk I/O require ETW.

## Capture the stationary prologue

Use the usual diagnostic Release build and usual launcher settings. Before the runner is started (in the actual CMD used, not necessarily Play-VIctory.cmd), add:

```cmd
set "VICTORY_TIMELINE=E:\GTA6PC\critical_path_138.json"
set "VICTORY_TIMELINE_MANUAL=1"
set "VICTORY_TIMELINE_FRAMES=6"
set "VICTORY_TIMELINE_SAMPLE=0"
```

Once the player is controllable and the benchmark is stable, hold F9 briefly until the timeline starts. It prepares the existing timestamp lists then captures six native title-present intervals, once per launch. Do not move or toggle settings. Wait until the log says "the timeline is written". Return critical_path_138.txt, critical_path_138.json, launch log and memory_audit.csv. The JSON is viewable in Perfetto. Normal runs without VICTORY_TIMELINE do not wrap waits or collect a trace. Manual mode defaults to disabling the existing aggressive title-thread suspend sampler. Capture itself adds overhead (timestamp command lists, wait wrappers, trace events); compare uncaptured FPS separately.

## Optional system scheduling / hard-fault trace

If installed, Windows Performance Recorder from an elevated terminal can capture system ETW in parallel: use `wpr -profiles` to identify available CPU, disk/file I/O and memory profiles, or select CPU usage, disk I/O and relevant memory profiles in WPRUI. Use file mode and stop after the short in-game capture, saving an ETL. Inspect CPU Usage (Precise), Hard Faults and Disk Usage in WPA. This is separate from the internal trace; no ETW session is started by the game. System ETL may contain private paths/process data; inspect locally before sharing.

Report limitations: an unobserved wake, partially captured recording, or missing GPU stamp remains unknown. Do not treat summed parallel durations as frametime. No performance gain is expected from this build; no Windows runtime verification performed here.
