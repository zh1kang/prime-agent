#!/bin/sh
set -eu

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
REPO="${PA_LOCAL_REPO:-$(CDPATH= cd -- "$SCRIPT_DIR/.." && pwd)}"
DEST="${PA_HARNESS_DIR:-$HOME/.local/share/pa-harness}"
BIN_DST="$DEST/prime-agent"

# PA_LOCAL_BIN names the binary to install. Otherwise pick the newest build
# across local, release, and debug profiles so an older profile never shadows
# a fresh build.
if [ -n "${PA_LOCAL_BIN:-}" ]; then
    BIN_SRC="$PA_LOCAL_BIN"
else
    BIN_SRC=""
    for candidate in \
        "$REPO/target/local/radiata" \
        "$REPO/target/release/radiata" \
        "$REPO/target/debug/radiata"; do
        if [ -f "$candidate" ]; then
            if [ -z "$BIN_SRC" ] || [ "$candidate" -nt "$BIN_SRC" ]; then
                BIN_SRC="$candidate"
            fi
        fi
    done
    if [ -z "$BIN_SRC" ]; then
        echo "install-local: no built binary under $REPO/target/{local,release,debug}; run: cargo build --profile local -p radiata-cli" >&2
        exit 1
    fi
fi

if [ ! -f "$BIN_SRC" ]; then
    echo "install-local: missing built binary at $BIN_SRC" >&2
    exit 1
fi
echo "install-local: installing $BIN_SRC"

mkdir -p "$DEST"

# Stage onto a fresh inode, ad-hoc sign, then atomically rename so macOS ARM64
# never kills the binary with a stale vnode code-signature cache entry.
TMP_BIN="$DEST/.prime-agent.tmp.$$"
trap 'rm -f "$TMP_BIN"' EXIT INT TERM
cp "$BIN_SRC" "$TMP_BIN"
chmod 755 "$TMP_BIN"
if command -v codesign >/dev/null 2>&1; then
    codesign -s - --force "$TMP_BIN" >/dev/null
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
    RADIATA_BIN="$(command -v radiata 2>/dev/null || echo "$HOME/.local/bin/radiata")"
    if [ -x "$RADIATA_BIN" ]; then
        "$RADIATA_BIN" shutdown >/dev/null 2>&1 || true
    fi
    pkill -f "thread captain watch" >/dev/null 2>&1 || true
fi
