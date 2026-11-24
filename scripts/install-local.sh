#!/bin/sh
set -eu

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
REPO="${PA_LOCAL_REPO:-$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)}"
DEST="${PA_HARNESS_DIR:-$HOME/.local/share/pa-harness}"
BIN_SRC="$REPO/target/debug/prime-agent"
BIN_DST="$DEST/prime-agent"

if [ ! -f "$BIN_SRC" ]; then
    echo "install-local: missing built binary at $BIN_SRC" >&2
    exit 1
fi

mkdir -p "$DEST"

# Stage onto a fresh inode, ad-hoc sign, then atomically rename so macOS ARM64
# never kills the binary with a stale vnode code-signature cache entry.
TMP_BIN="$DEST/.prime-agent.tmp.$$"
trap 'rm -f "$TMP_BIN"' EXIT INT TERM
cp "$BIN_SRC" "$TMP_BIN"
chmod 755 "$TMP_BIN"
if command -v codesign >/dev/null 2>&1; then
    codesign -s - --force "$TMP_BIN" >/dev/null 2>&1
fi
mv -f "$TMP_BIN" "$BIN_DST"
trap - EXIT INT TERM

# Sync Python runtime into both the harness dir and kernel-venv (if present).
if [ -d "$REPO/prime-agent-runtime/src" ]; then
    mkdir -p "$DEST/prime-agent-runtime/src"
    rsync -a --delete "$REPO/prime-agent-runtime/src/" "$DEST/prime-agent-runtime/src/"
    for rlm_dst in "$HOME/.prime/agent/kernel-venv"/lib/python*/site-packages/rlm; do
        if [ -d "$rlm_dst" ]; then
            rsync -a --delete --exclude='__pycache__' "$REPO/prime-agent-runtime/src/rlm/" "$rlm_dst/"
        fi
    done
fi

if [ "${1:-}" = "--restart" ]; then
    "$HOME/.local/bin/radiata" shutdown >/dev/null 2>&1 || true
    pkill -f "thread captain watch" >/dev/null 2>&1 || true
fi
