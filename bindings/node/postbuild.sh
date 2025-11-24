#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DIST_DIR="$REPO_ROOT/dist"

if [[ ! -d "$DIST_DIR" ]]; then
  echo "postbuild: dist directory not found" >&2
  exit 1
fi

cat "$REPO_ROOT/stub.d.ts" >> "$DIST_DIR/index.d.ts"
