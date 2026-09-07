#!/usr/bin/env bash
# Cross-compile mcp-shell-server.exe for Windows via Docker.
# Run from anywhere; output lands in <repo>/dist/mcp-shell-server.exe.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

docker buildx build \
  -f docker/windows-cross.Dockerfile \
  --output type=local,dest=./dist \
  .

echo "Built: $repo_root/dist/mcp-shell-server.exe"
