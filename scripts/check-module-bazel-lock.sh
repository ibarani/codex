#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# An unchanged lockfile does not establish that declared direct versions match the resolved graph.
if ! "${repo_root}/.github/scripts/run_bazel_with_buildbuddy.py" mod deps --lockfile_mode=error --check_direct_dependencies=error; then
  echo "Bazel dependency or lockfile validation failed."
  echo "Resolve the reported dependency errors; run 'just bazel-lock-update' if the lockfile needs refreshing."
  exit 1
fi
