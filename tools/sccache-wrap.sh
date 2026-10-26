#!/bin/sh
# Rust compiler wrapper for this workspace: route through sccache when it
# is installed, otherwise fall back to plain rustc (fresh checkouts and
# automation without sccache must keep building).
#
# Wired via .cargo/config.toml ([build] rustc-wrapper). Bypass with
# LTMRS_NO_SCCACHE=1 (escape hatch if a stale cache is ever suspected).
set -u
if [ "${LTMRS_NO_SCCACHE:-0}" = "1" ]; then
    _cc="$1"; shift; exec "$_cc" "$@"
fi
if command -v sccache >/dev/null 2>&1; then
    exec sccache "$@"
fi
if [ -n "${HOME:-}" ] && [ -x "$HOME/.cargo/bin/sccache" ]; then
    exec "$HOME/.cargo/bin/sccache" "$@"
fi
_cc="$1"; shift; exec "$_cc" "$@"
