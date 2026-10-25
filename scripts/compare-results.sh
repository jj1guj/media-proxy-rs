#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C

allow_sender_lag=false
if [[ ${1:-} == --allow-sender-lag ]]; then
    allow_sender_lag=true
    shift
fi
[[ $# == 2 ]] || { printf 'Usage: bash %s [--allow-sender-lag] BEFORE_DIR AFTER_DIR\n' "$0" >&2; exit 1; }
jq -n \
    --argjson allow_sender_lag "$allow_sender_lag" \
    --arg before "$1" --arg after "$2" \
    --slurpfile b "$1/summary.json" --slurpfile a "$2/summary.json" '
    $b[0] as $b | $a[0] as $a |
    ["manifest_sha256","rps","duration_s","max_inflight","request_timeout_s"] as $keys |
    [$keys[] as $key | select($b.config[$key] != $a.config[$key]) |
        "\($key): \($b.config[$key]) != \($a.config[$key])"] as $mismatches |
    def invalid($path;$run):
        if $run.valid_load == true then empty else
            "\($path): invalid load; unsent=\($run.metrics.dropped_before_send), " +
            "max sender lag=\($run.metrics.send_lag_max_ms)ms, " +
            "budget=\($run.metrics.send_lag_budget_ms)ms"
        end;
    [invalid($before;$b),invalid($after;$a)] as $invalid |
    def lag_only($run):
        $run.valid_load == false and
        $run.metrics.dropped_before_send == 0 and
        $run.metrics.completed == $run.config.scheduled and
        ($run.metrics.send_lag_max_ms | type) == "number" and
        ($run.metrics.send_lag_budget_ms | type) == "number" and
        $run.metrics.send_lag_max_ms > $run.metrics.send_lag_budget_ms;
    ([$b,$a] | all(.valid_load == true or lag_only(.))) as $can_override |
    if ($mismatches|length)>0 then
        error("Incompatible conditions: " + ($mismatches|join("; ")))
    elif ($invalid|length)>0 and ($allow_sender_lag and $can_override | not) then
        error($invalid|join("; "))
    else
    (if ($invalid|length)>0 then
        ("WARNING: comparing despite sender lag; results are not a validated fixed-rate comparison. " +
         ($invalid|join("; ")) + "\n" | stderr | empty)
     else empty end),
    (
    def row($name;$x;$y): [$name,$x,$y,(if $x==null or $y==null or $x==0 then null else ($y/$x-1)*100 end)];
    [["metric","before","after","change_percent"],
     row("success_rps";$b.metrics.success_rps;$a.metrics.success_rps),
     row("error_rate";$b.metrics.error_rate;$a.metrics.error_rate),
     row("p95_ms";$b.successful_latency.p95_ms;$a.successful_latency.p95_ms),
     row("p99_ms";$b.successful_latency.p99_ms;$a.successful_latency.p99_ms),
     row("rss_peak_bytes";$b.resources.rss_peak_bytes;$a.resources.rss_peak_bytes),
     row("cpu_mean_percent";$b.resources.cpu_mean_percent;$a.resources.cpu_mean_percent)]
    | .[] | @tsv) end' -r
