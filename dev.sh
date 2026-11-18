#!/usr/bin/env bash
# Run the Rust backend and the Next.js frontend together for local development.
# Ctrl+C tears down both processes cleanly.
#
# Usage: ./dev.sh

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Kill the whole process group on any exit signal so we never leave stragglers.
trap 'kill 0' SIGINT SIGTERM EXIT

# Backend: cargo run with info-level logging so market lifecycle is visible.
(
  cd "$ROOT/server"
  RUST_LOG=info cargo run -p exchange 2>&1 | sed -u 's/^/[backend]  /'
) &

# Frontend: Next.js dev server on :3000 (frontend/ is the app; its packages/ are workspace libraries).
# Backend URLs come from frontend/.env.local (see frontend/.env.example).
(
  cd "$ROOT/frontend"
  pnpm dev 2>&1 | sed -u 's/^/[frontend] /'
) &

wait
