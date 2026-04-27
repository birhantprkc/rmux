#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
case "$TARGET_DIR" in
    /*) ;;
    *) TARGET_DIR="$ROOT/$TARGET_DIR" ;;
esac

RMUX="$TARGET_DIR/debug/rmux"
reference_DIR="${RMUX_reference_DIR:-$ROOT/target/reference}"
SIGN_OFF="${RMUX_reference_SIGNOFF:-$(whoami)@$(hostname)}"

log() {
    printf '[macos-smoke] %s\n' "$*"
}

fail() {
    printf '[macos-smoke] ERROR: %s\n' "$*" >&2
    exit 1
}

if [[ "$(uname -s)" != "Darwin" ]]; then
    fail "scripts/smoke-macos.sh must run on macOS; got $(uname -s)"
fi

mkdir -p "$reference_DIR"
timestamp="$(date -u +%Y%m%d-%H%M%S)"
reference="$reference_DIR/macos-smoke-$timestamp.txt"

append() {
    printf '%s\n' "$*" >>"$reference"
}

append_command_result() {
    local status="$1"
    local command="$2"
    local output="$3"

    append "### $command"
    append
    append "- Status: $status"
    append
    append '```text'
    append "$output"
    append '```'
    append
}

run_capture() {
    local command_text="$1"
    shift

    log "$command_text"
    local output status
    set +e
    output="$("$@" 2>&1)"
    status=$?
    set -e

    if [[ "$status" -eq 0 ]]; then
        append_command_result "PASS" "$command_text" "$output"
    else
        append_command_result "FAIL ($status)" "$command_text" "$output"
        fail "$command_text failed; reference: $reference"
    fi
}

cd "$ROOT"

append "# RMUX macOS Smoke reference"
append
append "- Timestamp UTC: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
append "- Sign-off: $SIGN_OFF"
append "- Git HEAD: $(git rev-parse HEAD)"
append "- reference file: $reference"
append
append "## Host"
append
append '```text'
{
    sw_vers
    uname -a
    sysctl kern.ostype kern.osrelease kern.osversion hw.model hw.machine 2>/dev/null
} >>"$reference"
append '```'
append

run_capture "cargo build --locked" cargo build --locked
run_capture "cargo test -p rmux-pty --locked" cargo test -p rmux-pty --locked
run_capture "scripts/smoke-unix.sh" bash scripts/smoke-unix.sh
run_capture "scripts/smoke-unix-deep.sh" bash scripts/smoke-unix-deep.sh
run_capture "rmux diagnose --json" "$RMUX" diagnose --json

append "## Verdict"
append
append "PASS"

log "macOS smoke passed"
log "reference=$reference"
