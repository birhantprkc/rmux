#!/usr/bin/env bash
set -euo pipefail

iterations=30
line_count=10000
binary="target/release/rmux"
output_dir="target/perf"
skip_build=0
fail_on_budget=0

while [ "$#" -gt 0 ]; do
    case "$1" in
        --iterations)
            iterations="$2"
            shift 2
            ;;
        --line-count)
            line_count="$2"
            shift 2
            ;;
        --binary)
            binary="$2"
            shift 2
            ;;
        --output-dir)
            output_dir="$2"
            shift 2
            ;;
        --skip-build)
            skip_build=1
            shift
            ;;
        --fail-on-budget)
            fail_on_budget=1
            shift
            ;;
        -h|--help)
            cat <<'USAGE'
usage: scripts/perf-bench.sh [--iterations N] [--line-count N]
                             [--binary PATH] [--output-dir DIR]
                             [--skip-build] [--fail-on-budget]
USAGE
            exit 0
            ;;
        *)
            echo "unknown option: $1" >&2
            exit 2
            ;;
    esac
done

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

if [ "$skip_build" -eq 0 ]; then
    cargo build --locked --release
fi

if [ ! -x "$binary" ]; then
    echo "rmux binary was not found or is not executable: $binary" >&2
    exit 1
fi

binary="$(cd "$(dirname "$binary")" && pwd)/$(basename "$binary")"
mkdir -p "$output_dir"

metric_names=()
metric_p50=()
metric_p95=()
metric_mean=()
metric_min=()
metric_max=()
metric_budget=()
metric_status=()
metric_samples=()

unique_socket() {
    local metric="$1"
    printf 'rmux-perf-%s-%s-%s' "$metric" "$$" "$(date +%s%N)"
}

cleanup_socket() {
    local socket="$1"
    "$binary" -L "$socket" kill-server >/dev/null 2>&1 || true
}

run_timed_ms() {
    local output_file="$1"
    shift
    local start_ns end_ns status
    start_ns="$(date +%s%N)"
    set +e
    "$binary" "$@" >"$output_file" 2>&1
    status=$?
    set -e
    end_ns="$(date +%s%N)"
    if [ "$status" -ne 0 ]; then
        cat "$output_file" >&2
        echo "rmux $* failed with exit code $status" >&2
        exit "$status"
    fi
    awk -v start="$start_ns" -v end="$end_ns" 'BEGIN { printf "%.3f", (end - start) / 1000000 }'
}

percentile() {
    local percentile_value="$1"
    shift
    printf '%s\n' "$@" | sort -n | awk -v p="$percentile_value" '
        NF { values[++count] = $1 }
        END {
            idx = int((p / 100) * count)
            if (((p / 100) * count) > idx) {
                idx++
            }
            if (idx < 1) {
                idx = 1
            }
            printf "%.3f", values[idx]
        }
    '
}

mean_value() {
    printf '%s\n' "$@" | awk '{ sum += $1; count++ } END { printf "%.3f", sum / count }'
}

min_value() {
    printf '%s\n' "$@" | sort -n | head -n 1
}

max_value() {
    printf '%s\n' "$@" | sort -n | tail -n 1
}

new_line_script() {
    local path
    path="$(mktemp "${TMPDIR:-/tmp}/rmux-perf-lines.XXXXXX.sh")"
    cat >"$path" <<SCRIPT
#!/bin/sh
i=1
while [ "\$i" -le "$line_count" ]; do
    printf 'rmux-perf-line-%s\n' "\$i"
    i=\$((i + 1))
done
sleep 30
SCRIPT
    chmod +x "$path"
    printf '%s' "$path"
}

wait_for_line_marker() {
    local socket="$1"
    local output_file="$2"
    local marker="rmux-perf-line-$line_count"
    local deadline=$((SECONDS + 15))

    while [ "$SECONDS" -lt "$deadline" ]; do
        "$binary" -L "$socket" capture-pane -t perf -p -S "-$line_count" >"$output_file" 2>&1
        if grep -Fq "$marker" "$output_file"; then
            return 0
        fi
        sleep 0.1
    done

    echo "timed out waiting for pane output marker $marker" >&2
    return 1
}

sample_diagnose() {
    local output_file
    output_file="$(mktemp)"
    run_timed_ms "$output_file" diagnose --json
    rm -f "$output_file"
}

sample_new_session_sh() {
    local socket output_file
    socket="$(unique_socket new-sh)"
    output_file="$(mktemp)"
    run_timed_ms "$output_file" -L "$socket" new-session -d -s perf /bin/sh
    cleanup_socket "$socket"
    rm -f "$output_file"
}

sample_split_window_sh() {
    local socket output_file
    socket="$(unique_socket split-sh)"
    output_file="$(mktemp)"
    "$binary" -L "$socket" new-session -d -s perf /bin/sh >/dev/null
    run_timed_ms "$output_file" -L "$socket" split-window -d -t perf /bin/sh
    cleanup_socket "$socket"
    rm -f "$output_file"
}

sample_send_keys() {
    local socket output_file
    socket="$(unique_socket send-keys)"
    output_file="$(mktemp)"
    "$binary" -L "$socket" new-session -d -s perf /bin/sh >/dev/null
    run_timed_ms "$output_file" -L "$socket" send-keys -t perf RMUX_PERF Enter
    cleanup_socket "$socket"
    rm -f "$output_file"
}

sample_resize_pane() {
    local socket output_file
    socket="$(unique_socket resize)"
    output_file="$(mktemp)"
    "$binary" -L "$socket" new-session -d -s perf /bin/sh >/dev/null
    run_timed_ms "$output_file" -L "$socket" resize-pane -t perf -x 100
    cleanup_socket "$socket"
    rm -f "$output_file"
}

sample_pane_output_ready() {
    local socket output_file script start_ns end_ns
    socket="$(unique_socket output-ready)"
    output_file="$(mktemp)"
    script="$(new_line_script)"
    "$binary" -L "$socket" new-session -d -s perf /bin/sh "$script" >/dev/null
    start_ns="$(date +%s%N)"
    wait_for_line_marker "$socket" "$output_file"
    end_ns="$(date +%s%N)"
    cleanup_socket "$socket"
    rm -f "$output_file" "$script"
    awk -v start="$start_ns" -v end="$end_ns" 'BEGIN { printf "%.3f", (end - start) / 1000000 }'
}

sample_capture_pane() {
    local socket output_file script marker ms
    socket="$(unique_socket capture)"
    output_file="$(mktemp)"
    script="$(new_line_script)"
    marker="rmux-perf-line-$line_count"
    "$binary" -L "$socket" new-session -d -s perf /bin/sh "$script" >/dev/null
    wait_for_line_marker "$socket" "$output_file"
    ms="$(run_timed_ms "$output_file" -L "$socket" capture-pane -t perf -p -S "-$line_count")"
    if ! grep -Fq "$marker" "$output_file"; then
        echo "capture output did not include $marker" >&2
        exit 1
    fi
    cleanup_socket "$socket"
    rm -f "$output_file" "$script"
    printf '%s' "$ms"
}

record_metric() {
    local name="$1"
    local budget="$2"
    local sampler="$3"
    local samples=()

    echo "measuring $name ($iterations runs)" >&2
    for ((run = 1; run <= iterations; run++)); do
        samples+=("$("$sampler")")
    done

    local p50 p95 mean min max status
    p50="$(percentile 50 "${samples[@]}")"
    p95="$(percentile 95 "${samples[@]}")"
    mean="$(mean_value "${samples[@]}")"
    min="$(min_value "${samples[@]}")"
    max="$(max_value "${samples[@]}")"
    status="informational"
    if [ "$budget" != "null" ]; then
        status="$(awk -v p95="$p95" -v budget="$budget" 'BEGIN { if (p95 <= budget) print "pass"; else print "fail" }')"
    fi

    metric_names+=("$name")
    metric_p50+=("$p50")
    metric_p95+=("$p95")
    metric_mean+=("$mean")
    metric_min+=("$min")
    metric_max+=("$max")
    metric_budget+=("$budget")
    metric_status+=("$status")
    metric_samples+=("$(IFS=,; echo "${samples[*]}")")
}

record_metric "diagnose_json_cold" "null" sample_diagnose
record_metric "new_session_detached_sh" "500" sample_new_session_sh
record_metric "split_window_detached_sh" "150" sample_split_window_sh
record_metric "send_keys_detached_round_trip" "20" sample_send_keys
record_metric "resize_pane_round_trip" "100" sample_resize_pane
record_metric "pane_output_10k_ready" "null" sample_pane_output_ready
record_metric "capture_pane_10k_lines" "75" sample_capture_pane

timestamp="$(date -u +%Y%m%d-%H%M%S)"
json_path="$output_dir/unix-$timestamp.json"
markdown_path="$output_dir/unix-$timestamp.txt"

{
    printf '{\n'
    printf '  "schema": 1,\n'
    printf '  "timestamp": "%s",\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    printf '  "platform": "%s",\n' "$(uname -s | tr '[:upper:]' '[:lower:]')"
    printf '  "binary": "%s",\n' "$binary"
    printf '  "iterations": %s,\n' "$iterations"
    printf '  "line_count": %s,\n' "$line_count"
    printf '  "metrics": [\n'
    for ((i = 0; i < ${#metric_names[@]}; i++)); do
        [ "$i" -gt 0 ] && printf ',\n'
        printf '    {"name":"%s","p50_ms":%s,"p95_ms":%s,"mean_ms":%s,"min_ms":%s,"max_ms":%s,"budget_p95_ms":%s,"status":"%s","samples_ms":[%s]}' \
            "${metric_names[$i]}" "${metric_p50[$i]}" "${metric_p95[$i]}" \
            "${metric_mean[$i]}" "${metric_min[$i]}" "${metric_max[$i]}" \
            "${metric_budget[$i]}" "${metric_status[$i]}" "${metric_samples[$i]}"
    done
    printf '\n  ]\n'
    printf '}\n'
} >"$json_path"

{
    printf '# RMUX Unix Performance Bench\n\n'
    printf -- '- Binary: `%s`\n' "$binary"
    printf -- '- Iterations: `%s`\n' "$iterations"
    printf -- '- Line count: `%s`\n\n' "$line_count"
    printf '| Metric | p50 ms | p95 ms | Budget p95 ms | Status |\n'
    printf '|---|---:|---:|---:|---|\n'
    for ((i = 0; i < ${#metric_names[@]}; i++)); do
        budget="${metric_budget[$i]}"
        [ "$budget" = "null" ] && budget=""
        printf '| %s | %s | %s | %s | %s |\n' \
            "${metric_names[$i]}" "${metric_p50[$i]}" "${metric_p95[$i]}" \
            "$budget" "${metric_status[$i]}"
    done
} >"$markdown_path"

printf 'json=%s\n' "$json_path"
printf 'summary=%s\n' "$markdown_path"

if [ "$fail_on_budget" -eq 1 ]; then
    failed=0
    for status in "${metric_status[@]}"; do
        [ "$status" = "fail" ] && failed=1
    done
    if [ "$failed" -eq 1 ]; then
        echo "performance budget failed" >&2
        exit 1
    fi
fi
