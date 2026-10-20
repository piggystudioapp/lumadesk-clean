# Reproducible Windows performance smoke benchmark

LumaDesk includes `scripts/benchmark.ps1`, which creates synthetic files and an **isolated** `%APPDATA%` layout. It starts a release binary, observes visible desktop collection windows, waits for initial drawing/icon loads, and samples process CPU and memory while idle. The script does not modify the user's real layout.

```powershell
cargo build --release --locked
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/benchmark.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/benchmark.ps1 -Collections 4 -IdleSeconds 5
```

LumaDesk must be closed before running this script. Run it in an interactive Windows Explorer desktop session; Windows CI runners may not provide the required shell hierarchy. CPU percentages are normalized to a **single logical core**. Startup timing runs from launch request to the expected number of visible fence HWNDs, not first rendered frame, and does not represent a true cold boot.

## Local developer-host observations, 2026-10-19

Windows 11 x64, optimized Rust release build, warm filesystem cache and a desktop with one attached display. All pins are distinct local synthetic files. Results are point-in-time observations, not hardware-independent performance guarantees.

| Scenario | Time to fence HWNDs | Idle CPU (one core) | Working set | Private memory |
| --- | ---: | ---: | ---: | ---: |
| 1 fence × 160 pins, 8s idle | 187.5 ms | Below sample resolution (0.000%) | 47.7 MB | 56.8 MB |
| 4 fences × 160 pins, 5s idle | 256.5 ms | Below sample resolution (0.000%) | 64.2 MB | 102.8 MB |

Startup readings vary with warm caches and scheduler load; this single pair of samples should not be used to estimate scaling or claim a cold-start bound. This benchmark does not yet measure animation frame time, first visible paint, cold disk cache, slow network paths, monitor hotplug, mixed DPI, screen-reader behavior, or long-running memory stability. Those still require dedicated validation before broader release claims.
