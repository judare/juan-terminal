# Performance

Targets: < 150 MB idle, ~0% CPU idle, fast startup, smooth scrolling. Measured, not assumed.

## How to measure

```sh
cargo build --release
JUAN_TERMINAL_TRACE_STARTUP=1 ./target/release/juan-terminal   # prints time to window open
PID=$(pgrep -n juan-terminal)
top -l 4 -s 5 -pid $PID -stats pid,cpu,mem,threads   # MEM = physical footprint (what Activity Monitor shows)
footprint $PID                                       # detailed memory footprint
sample $PID 3                                        # what threads do while "idle"
```

Use an isolated state dir for reproducible runs: `JUAN_TERMINAL_STATE_DIR=/tmp/jt ./target/release/juan-terminal`.

## Results — M1 (Apple M3, macOS 27.0.1, release build)

| Metric | Value | Notes |
|---|---|---|
| Release binary | 4.9 MB | thin LTO, stripped |
| Startup to window | 0.88 s | cold, includes runtime Metal shader compilation (D2) |
| Idle, 1 terminal, RSS | 51 MB | `ps` |
| Idle, 1 terminal, footprint | 76 MB | `top` MEM |
| Idle CPU, 1 terminal | ~0.6 % | no cursor blink, no timers; to investigate (CVDisplayLink?) |
| 3 terminals, in use | 143 MB footprint | during interactive use |

Threads at idle: main, NSEvent, CVDisplayLink, async-io, GPUI executor pool, one `PTY reader`
per terminal (blocked in kqueue).

## To measure in next milestones
CPU during heavy output (`yes | head -n 2000000`, `cat` large file), scrolling, RAM with many
repositories, RAM with 10+ terminals, Git operations, syntax highlighting.
