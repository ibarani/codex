# codex-utils-pty

Lightweight helpers for spawning interactive processes either under a PTY (pseudo terminal) or regular pipes. The public API is minimal and mirrors both backends so callers can switch based on their needs (e.g., enabling or disabling TTY).

## API surface

- `spawn_pty_process(program, args, cwd, env, arg0, size)` → `SpawnedProcess`
- `spawn_pipe_process(program, args, cwd, env, arg0)` → `SpawnedProcess`
- `spawn_pipe_process_no_stdin(program, args, cwd, env, arg0)` → `SpawnedProcess`
- `combine_output_receivers(stdout_rx, stderr_rx)` → `broadcast::Receiver<Vec<u8>>`
- `conpty_supported()` → `bool` (Windows only; always true elsewhere)
- `TerminalSize { rows, cols }` selects PTY dimensions in character cells.
- `ProcessHandle` exposes:
  - `writer_sender()` → `mpsc::Sender<Vec<u8>>` (stdin)
  - `resize(TerminalSize)`
  - `close_stdin()`
  - `request_terminate()` → `io::Result<()>`, leaving output readers active.
  - `signal(ProcessSignal)` → `io::Result<()>`.
  - `has_exited()`, `exit_code()`, `terminate()`
- `SpawnedProcess` bundles `session`, `stdout_rx`, `stderr_rx`, and `exit_rx` (oneshot exit code).

Termination requests and observed exit are separate: `Ok(())` acknowledges the
request, while `exit_rx` and output-channel closure establish actual completion.
A failed request preserves its error kind and the terminator for an explicit
retry. Backend error text is not propagated. Successful termination consumes
the terminator, so repeated requests and later destruction do not send it again.
Unix interrupts retain the terminator; successful Windows interrupts consume it
because those supported non-PTY backends terminate on interrupt. A poisoned
terminator lock returns an error without invoking the backend.

`terminate()` and destruction remain best-effort cleanup: they request
termination and abort I/O helpers, reporting failures to stderr using a fixed
message and the error kind. Diagnostic write failure does not interrupt cleanup.
They do not promise successful termination, preserve output, or retry indefinitely.
Callers requiring a recoverable error or complete output use `request_terminate()`
and observe exit/output separately.

## Usage examples

```rust
use std::collections::HashMap;
use std::path::Path;
use codex_utils_pty::combine_output_receivers;
use codex_utils_pty::spawn_pty_process;
use codex_utils_pty::TerminalSize;

# tokio_test::block_on(async {
let env_map: HashMap<String, String> = std::env::vars().collect();
let spawned = spawn_pty_process(
    "bash",
    &["-lc".into(), "echo hello".into()],
    Path::new("."),
    &env_map,
    &None,
    TerminalSize::default(),
).await?;

let writer = spawned.session.writer_sender();
writer.send(b"exit\n".to_vec()).await?;

// Collect output until the process exits.
let mut output_rx = combine_output_receivers(spawned.stdout_rx, spawned.stderr_rx);
let mut collected = Vec::new();
while let Ok(chunk) = output_rx.try_recv() {
    collected.extend_from_slice(&chunk);
}
let exit_code = spawned.exit_rx.await.unwrap_or(-1);
# let _ = (collected, exit_code);
# anyhow::Ok(())
# });
```

Swap in `spawn_pipe_process` for a non-TTY subprocess; the rest of the API stays the same.
Use `spawn_pipe_process_no_stdin` to force stdin closed (commands that read stdin will see EOF immediately).

## Tests

Unit tests live in `src/lib.rs` and cover both backends (PTY Python REPL and pipe-based stdin roundtrip). Run with:

```
just test -p codex-utils-pty --no-capture
```
