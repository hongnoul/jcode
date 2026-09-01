#!/usr/bin/env bash
set -euo pipefail
# Keep local fork + device on upstream + local patch stack.
# Usage: ./scripts/sync-fork.sh [--push] [--build]
#   --push: also push fork/master
#   --build: also selfdev build after rebase
# Defaults to rebase only (dry: shows what would happen).

PUSH=0
BUILD=0
for arg in "$@"; do
  case "$arg" in
    --push) PUSH=1 ;;
    --build) BUILD=1 ;;
    --all) PUSH=1; BUILD=1 ;;
    -h|--help) echo "Usage: $0 [--push] [--build] [--all]"; exit 0 ;;
    *) echo "Unknown arg: $arg" >&2; exit 1 ;;
  esac
done

cd "$(git rev-parse --show-toplevel)"

if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "Dirty worktree - commit or stash first:" >&2
  git status --short
  exit 1
fi

echo "Fetching origin + fork..."
git fetch origin --prune --quiet
git fetch fork --prune --quiet 2>/dev/null || true

BEHIND=$(git rev-list --count HEAD..origin/master 2>/dev/null || echo 0)
AHEAD=$(git rev-list --count origin/master..HEAD 2>/dev/null || echo 0)
echo "Local: +$AHEAD ahead, -$BEHIND behind origin/master"
if [ "$BEHIND" -eq 0 ]; then
  echo "Already up to date on upstream."
else
  echo "Rebasing $BEHIND upstream commits..."
  git rebase origin/master
  echo "Rebase done."
fi

if [ "$PUSH" -eq 1 ]; then
  echo "Pushing to fork (force-with-lease)..."
  git push fork HEAD:master --force-with-lease
  echo "Pushed."
fi

if [ "$BUILD" -eq 1 ]; then
  echo "Building (selfdev tui)..."
  # Use jcode's selfdev if available, else cargo
  if command -v jcode >/dev/null 2>&1 && jcode self-dev --help >/dev/null 2>&1; then
    REASON="sync-fork: +$(git rev-list --count origin/master..HEAD) on $(git rev-parse --short HEAD)"
    jcode self-dev --build --reason "$REASON" --target tui 2>&1 | tail -n 30 || cargo build -p jcode
  else
    cargo build -p jcode
  fi
  # Restart menubar so new icon/appearance takes effect
  pkill -f "jcode menubar" 2>/dev/null || true
  sleep 1
  nohup jcode menubar >/tmp/jcode-menubar.log 2>&1 &
  echo "Menubar restarted on $(jcode --version 2>&1 | head -n1)"
fi

echo "Done. Local ahead origin: $(git rev-list --count origin/master..HEAD)"
