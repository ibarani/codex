# codex-utils-cargo-bin runfiles strategy

We disable directory-based runfiles and use the manifest strategy on all
platforms, avoiding Windows path length issues. Bazel supplies
`RUNFILES_MANIFEST_ONLY` and `RUNFILES_MANIFEST_FILE`; the former selects the
runfiles branch and the latter supplies the manifest to the `runfiles` crate.

`cargo_bin` resolves an already built binary in this order:

- Under Bazel, resolve `CARGO_BIN_EXE_<name>` through the runfiles manifest.
- Otherwise, use an absolute `NEXTEST_BIN_EXE_<name>` path before
  `CARGO_BIN_EXE_<name>`. Nextest's runtime paths account for archive remapping.
  Exact names precede underscore aliases within each namespace; nextest
  aliases still precede Cargo exact keys. A present but invalid path fails;
  it never falls through to a different artifact.
- Without a per-binary path, retain the existing `assert_cmd` Cargo layout
  fallback. Cargo and nextest expose binary paths for the current package;
  tests that launch another workspace package need this fallback.

With separate intermediate and final output directories, export both
`CARGO_BUILD_BUILD_DIR` and `CARGO_TARGET_DIR` as absolute paths. The fallback
maps the candidate from the declared build root to the declared target root,
keeping its `profile/name` or `target/profile/name` suffix and executable
extension. Custom profiles and cross-target suffixes are not inferred from
hard-coded names. A missing file, mismatched root, parent traversal, relative
root or unknown layout fails visibly. No binary is built or executed during
lookup, and `PATH` is never searched.

This compatibility path supports Cargo's existing directory layout; it is
not a general parser for Cargo configuration or future internal build layouts.
Configurations supplied only in `.cargo/config.toml` must also expose the two
absolute roots when cross-package fallback needs relocation. Nextest archive
runs should use its exact per-binary paths; cross-package fallback requires
updated roots matching that run. Without a declared build root, the original
co-located-layout fallback remains in effect. The resolver does not verify an
artifact's build provenance or that a cross-target binary can run on the host.

`find_resource!` selects Bazel runfiles using `RUNFILES_MANIFEST_ONLY`; otherwise
it joins the resource to the calling crate's compile-time `CARGO_MANIFEST_DIR`.

The resolver tests cover the actual public cross-package lookup with a separate
final root, exact remapped-path precedence, invalid explicit paths, custom
profiles, target suffixes and rejected layouts. Fixtures are never executed.

References:

- https://bazel.build/docs/runfiles
- https://nexte.st/docs/configuration/env-vars/
- https://doc.rust-lang.org/cargo/reference/build-cache.html
