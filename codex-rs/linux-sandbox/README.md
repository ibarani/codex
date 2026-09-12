# codex-linux-sandbox

This crate is responsible for producing:

- a `codex-linux-sandbox` standalone executable for Linux that is bundled with the Node.js version of the Codex CLI
- a lib crate that exposes the business logic of the executable as `run_main()` so that
  - the `codex-exec` CLI can check if its arg0 is `codex-linux-sandbox` and, if so, execute as if it were `codex-linux-sandbox`
  - this should also be true of the `codex` multitool CLI

On Linux, Codex prefers the first `bwrap` found on `PATH`
outside the current working directory whenever it is available. If `bwrap` is
present but too old to support
`--argv0`, the helper keeps using system bubblewrap and switches to a
no-`--argv0` compatibility path for the inner re-exec. If `bwrap` is missing,
the helper falls back to the bundled `codex-resources/bwrap` binary shipped
with Codex.
Codex also surfaces a startup warning when `bwrap` is missing so users know it
is falling back to the bundled helper. Codex surfaces the same startup warning
path when bubblewrap cannot create user namespaces. WSL2 follows the normal
Linux bubblewrap path. WSL1 is not supported for bubblewrap sandboxing because
it cannot create the required user namespaces, so Codex rejects sandboxed shell
commands that would enter the bubblewrap path.

**Current Behavior**
- Legacy `SandboxPolicy` / `sandbox_mode` configs remain supported.
- Bubblewrap is the default filesystem sandbox.
- If `bwrap` is present on `PATH` outside the current working directory, the
  helper uses it.
- If `bwrap` is present but too old to support `--argv0`, the helper uses a
  no-`--argv0` compatibility path for the inner re-exec.
- If `bwrap` is missing, the helper falls back to the bundled
  `codex-resources/bwrap` path.
- If `bwrap` is missing, Codex also surfaces a startup warning instead of
  printing directly from the sandbox helper.
- If bubblewrap cannot create user namespaces, Codex surfaces a startup warning
  instead of waiting for a runtime sandbox failure.
- WSL2 uses the normal Linux bubblewrap path.
- WSL1 is not supported for bubblewrap sandboxing; Codex rejects sandboxed
  shell commands that would require the bubblewrap path before invoking `bwrap`.
- Legacy Landlock + mount protections remain available as an explicit legacy
  fallback path.
- Set `features.use_legacy_landlock = true` (or CLI `-c use_legacy_landlock=true`)
  to force the legacy Landlock fallback.
- The legacy Landlock fallback is used only when the split filesystem policy is
  sandbox-equivalent to the legacy model after `cwd` resolution.
- Split-only filesystem policies that do not round-trip through the legacy
  `SandboxPolicy` model stay on bubblewrap so nested read-only or denied
  carveouts are preserved.
- When network filtering is required with Bubblewrap, the helper passes its
  network filter through a sealed anonymous descriptor. Bubblewrap applies `PR_SET_NO_NEW_PRIVS` and that filter
  to both its namespace init and the command before execution. The same policy
  generator serves the in-process legacy path; filters are not duplicated.
- When bubblewrap is active, the filesystem is read-only by default via `--ro-bind / /`.
- When bubblewrap is active, writable directory roots are layered with `--bind <root> <root>`.
  A non-directory writable root is opened with `O_PATH`, classified through that descriptor,
  and mounted with `--bind-fd`; it receives no directory-only metadata masks.
  Bubblewrap verifies that the actual mount matches the pinned inode before starting the command.
  On older system Bubblewrap, the existing trusted inner stage performs that identity check and
  closes the inherited descriptor. Unknown metadata errors abort construction; a pathname-only
  type check is insufficient for omitting directory protection.
  This does not introduce a general guarantee about concurrent replacement of directory roots.
- When bubblewrap is active, protected subpaths under writable roots (for
  example `.git`,
  resolved `gitdir:`, and `.codex`) are re-applied as read-only via `--ro-bind`.
- When bubblewrap is active, overlapping split-policy
  entries are applied in path-specificity order so narrower writable children
  can reopen broader read-only or denied parents while narrower denied subpaths
  still win. For example, `/repo = write`, `/repo/a = none`, `/repo/a/b = write`
  keeps `/repo` writable, denies `/repo/a`, and reopens `/repo/a/b` as
  writable again.
- When bubblewrap is active, unreadable glob entries are expanded before
  launching the sandbox and matching files are masked in bubblewrap:

  ```text
  Prefer:   rg --files --hidden --no-ignore --glob <pattern> -- <search-root>
  Fallback: internal globset walker when rg is not installed
  Failure:  any other rg failure aborts sandbox construction
  ```

  Users can cap the scan depth per permissions profile:

  ```toml
  [permissions.workspace.filesystem]
  glob_scan_max_depth = 2

  [permissions.workspace.filesystem.":workspace_roots"]
  "**/*.env" = "none"
  ```

- When bubblewrap is active, symlink-in-path and non-existent protected paths inside
  writable roots are blocked by mounting `/dev/null` on the symlink or first
  missing component.
- Bubblewrap retains its own namespace init to reap children and terminate the
  namespace when its outer monitor exits. The trusted inner Codex stage verifies
  inherited mounts and capabilities and finishes proxy handoff, then replaces itself with
  the command; it does not replace PID 1.
  This preserves parent-death cleanup across command startup and cancellation,
  including hosts where an executable transition clears the inherited death signal.
  When the initial command exits, Bubblewrap reports its status and terminates
  remaining descendants through that same namespace owner.
- When bubblewrap is active, the helper explicitly isolates the user namespace via
  `--unshare-user` and the PID namespace via `--unshare-pid`.
- When bubblewrap is active and network is restricted without proxy routing, the helper also
  isolates the network namespace via `--unshare-net`.
- In managed proxy mode, the helper uses `--unshare-net` plus an internal
  TCP->UDS->TCP routing bridge so tool traffic reaches only configured proxy
  endpoints.
- In managed proxy mode, after the bridge is live, seccomp blocks new
  AF_UNIX/socketpair creation for the user command.
- When bubblewrap is active, it mounts a fresh `/proc` via `--proc /proc` by default, but
  you can skip this in restrictive container environments with `--no-proc`.

**Notes**
- The CLI surface is `codex sandbox`; the host OS selects the sandbox backend.
