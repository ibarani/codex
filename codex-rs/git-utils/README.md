# codex-git-utils

Helpers for interacting with git, including patch application. The crate also
exposes a lightweight baseline API for internal directories that use git only
as a resettable diff mechanism: `ensure_git_baseline_repository` preserves a
usable `root/.git` baseline or creates one when it is missing or unusable,
`reset_git_repository` replaces `root/.git` with a fresh one-commit baseline,
and `diff_since_latest_init` returns structured file changes plus a unified
diff from that baseline to the current directory contents.

```rust,no_run
use std::path::Path;

use codex_git_utils::{apply_git_patch, ApplyGitRequest};

let repo = Path::new("/path/to/repo");

// Apply a patch (omitted here) to the repository.
let request = ApplyGitRequest {
    cwd: repo.to_path_buf(),
    diff: String::from("...diff contents..."),
    revert: false,
    preflight: false,
};
let result = apply_git_patch(&request)?;
```

`run_git_command_with_cancellation` accepts an optional command deadline and a
cancellation future. It drains stdout and stderr concurrently before waiting for
exit, so a descendant that retains a pipe cannot leave a cached process-group
identity armed after the root is reaped. Cancellation is checked before spawning.
Cancellation, timeout and I/O failure request termination using the retained
child, with a separate two-second allowance to confirm the direct child's exit.
An absent command deadline leaves duration uncapped; cancellation still applies.
A dropped future requests the same owned termination; it cannot await cleanup.
The existing internal timeout API uses this owner and keeps its `Option<Output>`
contract for Git information queries. Successful output/status semantics and
Windows contained-job/fallback behavior are retained. The owner does not claim
to contain descendants that deliberately leave its Unix process group, or to
confirm every descendant exit. Windows and non-Linux execution require their
own qualification.
