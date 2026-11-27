# Build 0.139 — reliable manual timeline trigger

The 0.138 manual capture tested F9 only on title presents, so a brief press could be missed. No latest log was available to establish which stage failed. The existing CMD settings are valid. In a diagnostic build, the profiler now announces its configured output and trigger on the first title present and accepts a persistent, focus-independent file request. It consumes that file before starting. Use the existing manual settings, then create an empty file with the same base name as the JSON and the extension .trigger, e.g.:

```cmd
type nul > "E:\GTA6PC\critical_path_138.trigger"
```

Run this from another CMD or Win+R (prefix with cmd /c) when in the stationary prologue. The filename follows VICTORY_TIMELINE, or VICTORY_TIMELINE_TRIGGER if provided. Do not leave a stale trigger before launching; it would start capture at the first present. The F9 route remains available. New log milestones distinguish armed, request accepted, preparation complete, capture complete and report construction. A plain release without diagnostics cannot capture. No rendering/performance changes. Windows runtime not verified.
