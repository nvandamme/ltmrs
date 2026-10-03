#!/usr/bin/env bash
# P2-2 reproducible release evidence (sanitized, tied to the tested commit).
#
# Usage: tools/gen_release_evidence.sh [--quick]
#   --quick: fmt + manifest only (seconds; used by tests/release_evidence.rs).
#   default (full): also clippy + full test suite (minutes).
#
# Output: reports/release-<short-sha>/ with manifest.json, deps.txt and
# (full mode) suite/clippy logs. The bundle dir is printed on the last
# stdout line for harness consumption. Everything recorded is sanitized:
# digests, versions, counts and exit codes — no memory contents, no secrets,
# no absolute local paths in the manifest.
set -euo pipefail

MODE="full"
if [ "${1:-}" = "--quick" ]; then
    MODE="quick"
fi

HEAD="$(git rev-parse HEAD)"
SHORT="${HEAD:0:12}"
DIR="reports/release-${SHORT}"
mkdir -p "$DIR"

# Format gate (seconds).
if cargo fmt -- --check > "$DIR/fmt.log" 2>&1; then
    FMT_CLEAN="true"
else
    FMT_CLEAN="false"
fi

TOOLCHAIN="$(rustc --version)"
LOCK_SHA="$(sha256sum Cargo.lock | cut -d' ' -f1)"
# Direct dependency versions only (names + versions, no paths: the root
# package line carries its local path and is excluded).
cargo tree --depth 1 --prefix none --no-dev-dependencies 2>/dev/null \
    | grep -E '^[^ ]+ v[0-9]' \
    | grep -v ' (' \
    | sort -u > "$DIR/deps.txt" || true

CLIPPY_STATUS="null"
SUITE_SUMMARY="null"
if [ "$MODE" = "full" ]; then
    if cargo clippy --all-targets -- -D warnings > "$DIR/clippy.log" 2>&1; then
        CLIPPY_STATUS='"clean"'
    else
        CLIPPY_STATUS='"failed"'
    fi
    if cargo test --workspace --all-targets > "$DIR/suite.log" 2>&1; then
        SUITE_STATUS='"passed"'
    else
        SUITE_STATUS='"failed"'
    fi
    # Counts from the lib-suite line only (gitignored scratch detail stays
    # in suite.log; the manifest carries counts + exit status).
    LIB_LINE="$(grep -E '^test result: ok\. [0-9]+ passed' "$DIR/suite.log" | head -n 1 || true)"
    SUITE_SUMMARY="{\"status\": $SUITE_STATUS, \"lib_line\": \"${LIB_LINE:-unknown}\"}"
fi

python3 - "$DIR/manifest.json" "$HEAD" "$MODE" "$TOOLCHAIN" "$LOCK_SHA" "$FMT_CLEAN" "$CLIPPY_STATUS" "$SUITE_SUMMARY" "$DIR/deps.txt" <<'EOF'
import json, sys
_, out, head, mode, toolchain, lock_sha, fmt_clean, clippy_raw, suite_raw, deps = sys.argv
manifest = {
    "commit": head,
    "mode": mode,
    "toolchain": toolchain,
    "cargo_lock_sha256": lock_sha,
    "fmt_clean": (fmt_clean == "true"),
    "clippy": None if clippy_raw == "null" else json.loads(clippy_raw),
    "suite": None if suite_raw == "null" else json.loads(suite_raw),
    "direct_dependencies": open(deps).read().split(),
    "sanitized": True,
    "note": "Reproducible from HEAD via tools/gen_release_evidence.sh; no memory contents, secrets or local paths.",
}
json.dump(manifest, open(out, "w"), indent=2, sort_keys=True)
EOF

echo "$DIR"
