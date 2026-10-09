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
# The CHECKOUT's own committed script runs (relative invocation after cd),
# never the outer worktree's copy: tested code AND evidence procedure come
# from the exact same commit.
if [ -n "$CHECKOUT" ]; then
    rm -rf "$CHECKOUT"
    mkdir -p "$CHECKOUT"
    git worktree add --detach "$CHECKOUT" HEAD >&2
    trap 'git worktree remove --force "$CHECKOUT"' EXIT
    set +e
    # Full is the default mode, not a CLI flag: the parser only knows
    # --quick (full = absence of --quick). Build the recursive args
    # explicitly so the publishable clean-checkout path never emits the
    # unsupported `--full` flag. Locked resolution is always on here: a
    # clean-checkout bundle must stand on its own.
    args=()
    if [ "$MODE" = "quick" ]; then
        args+=(--quick)
    fi
    if [ -n "$RELEASE" ]; then
        args+=(--release)
    fi
    args+=(--locked)
    (cd "$CHECKOUT" && bash tools/gen_release_evidence.sh "${args[@]}")
    INNER=$?
    set -e
    # Move the bundle back even on failure (the logs are the evidence);
    # the checkout's reports/ is disposable. An inner failure before its
    # first bundle leaves no directory: report that instead of letting
    # `ls`/`mv` fail opaquely under `set -e`.
    NEWEST="$(ls -dt "$CHECKOUT"/reports/release-*/ 2>/dev/null | head -n 1 || true)"
    if [ -z "$NEWEST" ]; then
        echo "release evidence: inner run produced no bundle dir (exit $INNER)" >&2
        exit $INNER
    fi
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
# Effective linker provenance (mirrors tools/detect_linker.rs selection so
# two bundles built with different linkers cannot look identical): explicit
# LTMRS_LINKER wins, else the first of mold/wild the system driver accepts,
# else the system default (non-Linux included — no probing happens there).
LINKER_REQUESTED="${LTMRS_LINKER:-auto}"
LINKER_EFFECTIVE="system"
LINKER_VERSION="null"
if [ -n "${LTMRS_LINKER:-}" ] && [ "$LTMRS_LINKER" != "system" ]; then
    LINKER_EFFECTIVE="$LTMRS_LINKER"
elif [ -z "${LTMRS_LINKER:-}" ] && [ "$(uname -s)" = "Linux" ]; then
    CC_BIN="${CC:-cc}"
    for CAND in mold wild; do
        PROBE="$(mktemp)"
        if printf 'int main(){return 0;}' | "$CC_BIN" -fuse-ld="$CAND" -x c - -o "$PROBE" >/dev/null 2>&1 && [ -e "$PROBE" ]; then
            LINKER_EFFECTIVE="$CAND"
            rm -f "$PROBE"
            break
        fi
        rm -f "$PROBE"
    done
fi
if command -v "$LINKER_EFFECTIVE" >/dev/null 2>&1; then
    LINKER_VERSION="$("$LINKER_EFFECTIVE" --version 2>/dev/null | head -n 1)"
fi
# Workspace-wide direct-dependency inventory: the union of every member's
# direct deps (names + versions, no paths: local rows carry `(...)` and are
# excluded). `cargo tree --workspace` renders member roots as already-shown
# `(*)` stubs, so members are enumerated via cargo metadata instead — a new
# crate is picked up automatically, and member-owned pins (fjall, candle,
# lancedb, rmcp, ...) can no longer slip out of the evidence.
DEPS_TMP="$(mktemp)"
# shellcheck disable=SC2086
MEMBERS="$(cargo metadata $LOCKED --no-deps --format-version 1 2>/dev/null \
    | python3 -c 'import json,sys; print(" ".join(sorted(m["name"] for m in json.load(sys.stdin)["packages"])))'
)"
# shellcheck disable=SC2086
for MEMBER in $MEMBERS; do
    # Capture first: under `pipefail` a trailing `|| true` would also
    # swallow a `cargo tree` failure into an incomplete deps.txt, so the
    # tree run must succeed on its own and only the grep filter may
    # come up empty.
    TREE="$(cargo tree $LOCKED -p "$MEMBER" --depth 1 --prefix none --no-dev-dependencies 2>/dev/null)" || exit 1
    printf '%s\n' "$TREE" \
        | grep -E '^[^ ]+ v[0-9]' \
        | grep -v ' (' \
        >> "$DEPS_TMP" || true
done
sort -u "$DEPS_TMP" > "$DIR/deps.txt"
rm -f "$DEPS_TMP"

CLIPPY_STATUS="null"
SUITE_SUMMARY="null"
FAILED="false"
if [ "$MODE" = "full" ]; then
    # Workspace-wide Clippy gate (all members, all targets): in a
    # non-virtual workspace a bare root invocation selects only the root
    # package, so --workspace is load-bearing here.
    # shellcheck disable=SC2086
    if cargo clippy $LOCKED --workspace --all-targets -- -D warnings > "$DIR/clippy.log" 2>&1; then
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

# Locked runs must not resolve anything: every dependency-resolving
# invocation above obeyed --locked, so a changed lockfile means the
# recorded hash no longer describes what was tested. Fail loudly
# instead of publishing a manifest whose lock hash is a lie.
if [ -n "$LOCKED" ]; then
    LOCK_SHA_AFTER="$(sha256sum Cargo.lock | cut -d' ' -f1)"
    if [ "$LOCK_SHA_AFTER" != "$LOCK_SHA" ]; then
        echo "release evidence: Cargo.lock changed during locked run" >&2
        if [ "$MODE" = "full" ]; then
            FAILED="true"
        else
            exit 1
        fi
    fi
fi

python3 - "$DIR/manifest.json" "$HEAD" "$MODE" "$PROFILE" "$LOCKED_FLAG" "$TOOLCHAIN" "$LOCK_SHA" "$FMT_CLEAN" "$CLIPPY_STATUS" "$SUITE_SUMMARY" "$DIR/deps.txt" "$LINKER_REQUESTED" "$LINKER_EFFECTIVE" "$LINKER_VERSION" <<'EOF'
import json, sys
(_, out, head, mode, profile, locked, toolchain, lock_sha, fmt_clean,
 clippy_raw, suite_raw, deps, linker_requested, linker_effective,
 linker_version_raw) = sys.argv
def _dep_record(line):
    # deps.txt rows are `name version` (cargo tree --prefix none); a row
    # without a version keeps its name instead of breaking the manifest.
    parts = line.split(None, 1)
    return {"name": parts[0], "version": parts[1] if len(parts) == 2 else ""}
manifest = {
    "commit": head,
    "mode": mode,
    "profile": profile,
    "locked": (locked == "true"),
    "toolchain": toolchain,
    "requested_linker": linker_requested,
    "effective_linker": linker_effective,
    "effective_linker_version": None if linker_version_raw == "null" else linker_version_raw,
    "cargo_lock_sha256": lock_sha,
    "fmt_clean": (fmt_clean == "true"),
    "clippy": None if clippy_raw == "null" else json.loads(clippy_raw),
    "suite": None if suite_raw == "null" else json.loads(suite_raw),
    "direct_dependencies": [_dep_record(line) for line in open(deps).read().splitlines() if line.split()],
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
