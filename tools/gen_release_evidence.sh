#!/usr/bin/env bash
# P2-2 reproducible release evidence (sanitized, tied to the tested source).
#
# Usage: tools/gen_release_evidence.sh [--quick] [--release] [--locked] [--clean-checkout DIR]
#   --quick: fmt + manifest only (seconds; used by tests/release_evidence.rs).
#   default (full): also clippy + full test suite (minutes). Full mode exits
#     NONZERO when any required check fails, so CI/release gates can consume
#     the exit status (a manifest alone is a report, not a qualification).
#   --release: run the suite on the release profile (what release
#     qualification actually means here; dev-profile runs prove nothing
#     about the shipped artifact).
#   --locked: pass --locked to cargo invocations (locked resolution).
#   --clean-checkout DIR: generate from a detached clean checkout at HEAD
#     into DIR (publishable evidence with independently meaningful source
#     identity), instead of the working tree. The bundle is moved back into
#     this repo's reports/ afterwards and the checkout removed.
#
# Output: an EXCLUSIVE run directory reports/release-<short-sha>-<mode>-<id>/
# (mktemp: never shared, never overwritten; existing runs are immutable)
# with manifest.json, deps.txt and (full mode) suite/clippy logs. The bundle
# dir is printed on the last stdout line for harness consumption. Everything
# recorded is sanitized: digests, versions, counts and exit codes — no memory
# contents, no secrets, no absolute local paths in the manifest.
#
# Source identity is HEAD, full stop. No dirtiness tracking: evidence is
# generated during fixing rounds, so a dirty marker would always read true
# and could only ever block the release it is meant to serve. The honest
# contract is procedural — regenerate on a clean tree after commit —
# not a flag in the artifact.
set -euo pipefail

HERE="$(pwd)"
MODE="full"
RELEASE=""
LOCKED=""
CHECKOUT=""
while [ $# -gt 0 ]; do
    case "$1" in
        --quick) MODE="quick" ;;
        --release) RELEASE="--release" ;;
        --locked) LOCKED="--locked" ;;
        --clean-checkout)
            CHECKOUT="${2:?--clean-checkout requires a directory}"; shift ;;
        --clean-checkout=*) CHECKOUT="${1#--clean-checkout=}" ;;
        *) echo "unknown flag: $1" >&2; exit 2 ;;
    esac
    shift
done

# Publishable evidence from a detached clean checkout (re-review P2-2):
# a pristine HEAD worktree with locked resolution, so the artifact's source
# identity stands on its own instead of assuming a clean developer tree.
if [ -n "$CHECKOUT" ]; then
    rm -rf "$CHECKOUT"
    mkdir -p "$CHECKOUT"
    git worktree add --detach "$CHECKOUT" HEAD >&2
    trap 'git worktree remove --force "$CHECKOUT"' EXIT
    set +e
    (cd "$CHECKOUT" && bash "$HERE/tools/gen_release_evidence.sh" --$MODE $RELEASE $LOCKED --locked)
    INNER=$?
    set -e
    # Move the bundle back even on failure (the logs are the evidence);
    # the checkout's reports/ is disposable.
    NEWEST="$(ls -dt "$CHECKOUT"/reports/release-*/ | head -n 1)"
    mkdir -p reports
    mv "$NEWEST" reports/
    echo "reports/$(basename "$NEWEST")"
    exit $INNER
fi

HEAD="$(git rev-parse HEAD)"
SHORT="${HEAD:0:12}"
# The reports dir is gitignored: fresh checkouts don't have it.
mkdir -p reports
DIR="$(mktemp -d "reports/release-${SHORT}-${MODE}-XXXXXX")"

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
FAILED="false"
if [ "$MODE" = "full" ]; then
    # shellcheck disable=SC2086
    if cargo clippy $LOCKED --all-targets -- -D warnings > "$DIR/clippy.log" 2>&1; then
        CLIPPY_STATUS='"clean"'
    else
        CLIPPY_STATUS='"failed"'
        FAILED="true"
    fi
    if [ "$FMT_CLEAN" != "true" ]; then
        FAILED="true"
    fi
    # shellcheck disable=SC2086
    if cargo test $RELEASE $LOCKED --workspace --all-targets > "$DIR/suite.log" 2>&1; then
        SUITE_STATUS='"passed"'
    else
        SUITE_STATUS='"failed"'
        FAILED="true"
    fi
    # Counts from the lib-suite line only (gitignored scratch detail stays
    # in suite.log; the manifest carries counts + exit status).
    LIB_LINE="$(grep -E '^test result: ok\. [0-9]+ passed' "$DIR/suite.log" | head -n 1 || true)"
    SUITE_SUMMARY="{\"status\": $SUITE_STATUS, \"lib_line\": \"${LIB_LINE:-unknown}\"}"
fi
if [ -n "$RELEASE" ]; then
    PROFILE="release"
else
    PROFILE="dev"
fi
if [ -n "$LOCKED" ]; then LOCKED_FLAG=true; else LOCKED_FLAG=false; fi

python3 - "$DIR/manifest.json" "$HEAD" "$MODE" "$PROFILE" "$LOCKED_FLAG" "$TOOLCHAIN" "$LOCK_SHA" "$FMT_CLEAN" "$CLIPPY_STATUS" "$SUITE_SUMMARY" "$DIR/deps.txt" <<'EOF'
import json, sys
(_, out, head, mode, profile, locked, toolchain, lock_sha, fmt_clean,
 clippy_raw, suite_raw, deps) = sys.argv
manifest = {
    "commit": head,
    "mode": mode,
    "profile": profile,
    "locked": (locked == "true"),
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
# Full mode is a qualification gate, not just a report: fail loudly when
# any required check failed (quick mode only ever reports fmt).
if [ "$MODE" = "full" ] && [ "$FAILED" = "true" ]; then
    echo "release evidence: required checks failed (see $DIR)" >&2
    exit 1
fi
