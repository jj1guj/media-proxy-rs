#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null || die "Required command: $1"; }
positive() { awk -v n="$1" 'BEGIN {exit !(n ~ /^[0-9]+([.][0-9]+)?$/ && n > 0)}'; }
now() { printf '%s\n' "${EPOCHREALTIME/./}"; }

usage() {
    cat <<'EOF'
Linux / Bash 5 / curl / awk / jq / GNU coreutils required.
  bash scripts/benchmark.sh REQUESTS.tsv BASE_URL SECONDS RESULT_DIR [MAX_INFLIGHT]

Load is fixed at 49 RPS. This script does not connect to Prometheus.
Requests repeat in file order at fixed intervals, without retries.
Use extract-requests.sh to create the input,
and compare-results.sh to compare two result directories.
An existing output directory is never overwritten.
EOF
}

validate_manifest() {
    [[ -r $1 ]] || die "Cannot read manifest: $1"
    awk -F '\t' '
        NR==1 {
            if ($0=="id\tpath\turl\tflags") columns=4
            else if ($0=="id\tpath\turl\tflags\tsource_bytes\tsource_sha256") columns=6
            else exit 1
            next
        }
        NF!=columns || $1!=NR-1 || $2!~/^\/[^?#]*$/ || $3!~/^https?:\/\// {exit 1}
        $4!~/^(-|((static|emoji|avatar|preview|badge|fallback),?)+)$/ {exit 1}
        END {if (NR<2) exit 1}
    ' "$1" || die "Invalid or empty manifest: $1"
}

find_server_pid() {
    local base=$1 host port sockets line candidate
    local -A candidates=()
    host=${base#http://}
    port=80
    if [[ $host =~ :([0-9]+)$ ]]; then
        port=${BASH_REMATCH[1]}
        host=${host%:*}
    fi
    [[ $host != localhost ]] || host=127.0.0.1
    sockets=$(ss -H -ltnp "sport = :$port") || die "Cannot inspect listening sockets"
    while IFS= read -r line; do
        while [[ $line =~ pid=([0-9]+) ]]; do
            candidate=${BASH_REMATCH[1]}
            candidates[$candidate]=1
            line=${line#*"pid=$candidate"}
        done
    done < <(awk -v host="$host" -v port="$port" \
        '$4==host ":" port || $4=="0.0.0.0:" port || $4=="*:" port || $4=="[::]:" port {print}' <<< "$sockets")
    [[ ${#candidates[@]} == 1 ]] \
        || die "Cannot uniquely identify the server listening at $base; run as the server owner"
    for candidate in "${!candidates[@]}"; do
        [[ $(readlink "/proc/$candidate/exe") == */media-proxy-rs ]] \
            || die "The listener at $base is not media-proxy-rs"
        printf '%s\n' "$candidate"
    done
}

sample() {
    local pid=$1 start=$2 hz=$3 page=$4 output=$5 expected=$6
    local stat ticks rss identity timestamp
    printf 'elapsed_s\tcpu_ticks\trss_bytes\n' > "$output"
    while [[ -r /proc/$pid/stat ]]; do
        stat=$(< "/proc/$pid/stat") || return 1
        stat=${stat##*) }
        read -ra fields <<< "$stat"
        identity=${fields[19]}
        [[ $identity == "$expected" && ${fields[0]} != Z ]] || return 1
        ticks=$((${fields[11]} + ${fields[12]}))
        rss=$((${fields[21]} * page))
        timestamp=$(now)
        awk -v t="$timestamp" -v s="$start" -v c="$ticks" -v r="$rss" \
            'BEGIN {printf "%.6f\t%d\t%d\n",(t-s)/1e6,c,r}' >> "$output"
        sleep 0.2
    done
    return 1
}

request() {
    local seq=$1 id=$2 path=$3 url=$4 flags=$5 scheduled=$6
    local launched lag result rc=0 flag finished delta completed
    launched=${EPOCHREALTIME/./}
    delta=$((launched - scheduled))
    printf -v lag '%d.%03d' "$((delta / 1000))" "$((delta % 1000))"
    local args=(--get --data-urlencode "url=$url")
    IFS=',' read -ra flag_list <<< "$flags"
    for flag in "${flag_list[@]}"; do
        [[ $flag == - || -z $flag ]] || args+=(--data-urlencode "$flag=1")
    done
    result=$(curl --silent --show-error --globoff --path-as-is \
        --noproxy '*' --connect-timeout 5 --max-time "$request_timeout" \
        --header 'Accept: image/webp,image/*;q=0.8' \
        --output /dev/null --write-out $'%{http_code}\t%{size_download}\t%{time_starttransfer}\t%{time_total}\t%{content_type}' \
        "${args[@]}" -- "$base$path" 2>"$out/errors/$seq.txt") || rc=$?
    result=${result//$'\n'/ }
    [[ -s $out/errors/$seq.txt ]] || rm -f -- "$out/errors/$seq.txt"
    finished=${EPOCHREALTIME/./}
    delta=$((finished - start))
    printf -v completed '%d.%06d' "$((delta / 1000000))" "$((delta % 1000000))"
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$seq" "$id" "$rc" "$lag" "$result" \
        "$completed" > "$out/requests/$seq.tsv"
}

summarize() {
    local dir=$1 elapsed=$2 hz=$3 metric column
    local input_count
    input_count=$(jq -r '.input_requests' "$dir/config.json")
    awk -F '\t' -v count="$input_count" 'NR>1 {print int($1/count)+1,$8,$5,$3}' "$dir/requests.tsv" |
        sort -k1,1n -k2,2n |
        awk '
            function percentile(q, i) {
                i=int(ok*q);if(i<ok*q)i++
                return a[i]*1000
            }
            function emit() {
                if(!n)return
                printf "%d\t%d\t%d\t%d\t%s\t%s\t%s\n",cycle,n,ok,n-ok,
                    (ok?s*1000/ok:"-"),
                    (ok?percentile(.5):"-"),
                    (ok?percentile(.95):"-")
            }
            BEGIN {print "cycle\trequests\tsuccessful\terrors\tsuccess_mean_ms\tsuccess_p50_ms\tsuccess_p95_ms"}
            $1!=cycle {emit();delete a;n=0;ok=0;s=0;cycle=$1}
            {n++;if($4==0 && $3>=200 && $3<300){a[++ok]=$2;s+=$2}}
            END {emit()}
        ' > "$dir/cycles.tsv"
    awk -F '\t' -v elapsed="$elapsed" '
        NR==1 {next}
        {
            n++; bytes+=$6; lag+=$4
            if ($4>maxlag) maxlag=$4
            if ($3==0 && $5>=200 && $5<300) {
                ok++
            } else {errors++}
            if ($3==28) timeouts++
        }
        END {
            printf "{\"completed\":%d,\"successful\":%d,\"errors\":%d,\"timeouts\":%d,",n,ok,errors,timeouts
            printf "\"error_rate\":%.9f,\"completed_rps\":%.6f,\"success_rps\":%.6f,\"received_bytes_per_s\":%.6f,",errors/n,n/elapsed,ok/elapsed,bytes/elapsed
            printf "\"send_lag_mean_ms\":%.3f,\"send_lag_max_ms\":%.3f}",lag/n,maxlag
        }
    ' "$dir/requests.tsv" > "$dir/counts.json"
    for metric in latency ttfb; do
        column=8
        [[ $metric != ttfb ]] || column=7
        awk -F '\t' -v col="$column" 'NR>1 && $3==0 && $5>=200 && $5<300 {print $col*1000}' "$dir/requests.tsv" | sort -n |
            awk '{a[NR]=$1;sum+=$1}
                function p(q) {i=int(NR*q); if(i<NR*q)i++; return a[i]}
                END {if(!NR) print "null"; else printf "{\"samples\":%d,\"mean_ms\":%.3f,\"p50_ms\":%.3f,\"p95_ms\":%.3f,\"p99_ms\":%.3f,\"max_ms\":%.3f}\n",NR,sum/NR,p(.5),p(.95),p(.99),a[NR]}' > "$dir/$metric.json"
    done
    awk -F '\t' -v hz="$hz" '
        NR==1 {next}
        NR==2 {first=$1;initial=$2}
        {n++;sum+=$3;if($3>peak)peak=$3
         if(n>1 && $1>prev) {cpu=100*($2-ticks)/hz/($1-prev);if(cpu>maxcpu)maxcpu=cpu}
         prev=$1;ticks=$2}
        END {printf "{\"samples\":%d,\"rss_mean_bytes\":%.0f,\"rss_peak_bytes\":%.0f,\"cpu_seconds\":%.6f,\"cpu_mean_percent\":%.3f,\"cpu_peak_percent\":%.3f}\n",n,sum/n,peak,(ticks-initial)/hz,(prev>first?100*(ticks-initial)/hz/(prev-first):0),maxcpu}
    ' "$dir/resources.tsv" > "$dir/resources.json"
    jq -n --slurpfile config "$dir/config.json" --slurpfile counts "$dir/counts.json" \
        --slurpfile latency "$dir/latency.json" --slurpfile ttfb "$dir/ttfb.json" --slurpfile resources "$dir/resources.json" \
        '{config:$config[0],metrics:$counts[0],successful_latency:$latency[0],successful_ttfb:$ttfb[0],resources:$resources[0]}' > "$dir/summary.json"
    rm -- "$dir/counts.json" "$dir/latency.json" "$dir/ttfb.json" "$dir/resources.json"
}

run() {
    [[ $# == 4 || $# == 5 ]] || die "See --help for usage"
    local manifest=$1 base=${2%/} duration=$3 out=${4%/} limit=${5:-} pid
    local rps=49 request_timeout=30
    # Allow a full timeout window plus one second of worker startup/cleanup.
    [[ -n $limit ]] || limit=$((rps * (request_timeout + 1)))
    validate_manifest "$manifest"
    [[ $base =~ ^http://(127\.0\.0\.1|localhost|\[::1\])(:[0-9]+)?$ ]] \
        || die "Only a local HTTP server is supported (http://127.0.0.1:PORT)"
    need ss
    pid=$(find_server_pid "$base")
    [[ -r /proc/$pid/stat ]] || die "Server exited before measurement"
    positive "$rps" && positive "$duration" || die "RPS and seconds must be positive numbers"
    [[ $limit =~ ^[1-9][0-9]*$ ]] || die "MAX_INFLIGHT must be a positive integer"
    [[ ! -e $out ]] || die "Result path already exists: $out"
    local identity stat
    stat=$(< "/proc/$pid/stat"); stat=${stat##*) }; read -ra fields <<< "$stat"
    identity=${fields[19]}
    [[ $(readlink "/proc/$pid/exe") == */media-proxy-rs ]] || die "PID must be the media-proxy-rs server, not cargo"
    mkdir -p -- "$out/requests" "$out/errors"
    local sampler='' child
    trap 'for child in $(jobs -pr); do kill "$child" 2>/dev/null || :; done; wait || :' EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM
    local hz page start finish count total seq idx scheduled current delay inflight remaining dropped=0
    local -a active_jobs
    hz=$(getconf CLK_TCK); page=$(getconf PAGESIZE)
    local -a rows
    mapfile -t rows < <(tail -n +2 "$manifest")
    count=${#rows[@]}
    total=$(awk -v r="$rps" -v d="$duration" 'BEGIN {n=r*d;i=int(n);print i+(n>i)}')
    ((total > 0)) || die "No scheduled requests"
    local hash
    hash=$(sha256sum "$manifest"); hash=${hash%% *}
    jq -n --arg sha "$hash" --arg base "$base" --argjson pid "$pid" \
        --argjson rps "$rps" --argjson duration "$duration" --argjson total "$total" \
        --argjson inputs "$count" \
        --argjson timeout "$request_timeout" \
        --argjson max "$limit" --arg curl "$(curl --version | head -n 1)" \
        '{manifest_sha256:$sha,base_url:$base,pid:$pid,rps:$rps,duration_s:$duration,scheduled:$total,input_requests:$inputs,request_timeout_s:$timeout,max_inflight:$max,curl:$curl}' > "$out/config.json"
    start=${EPOCHREALTIME/./}
    sample "$pid" "$start" "$hz" "$page" "$out/resources.tsv" "$identity" &
    sampler=$!
    for ((seq=0; seq<total; seq++)); do
        scheduled=$((start + seq * 1000000 / rps))
        current=${EPOCHREALTIME/./}
        remaining=$((scheduled - current))
        if ((remaining > 0)); then
            printf -v delay '%d.%06d' "$((remaining / 1000000))" "$((remaining % 1000000))"
            sleep "$delay"
        fi
        kill -0 "$sampler" 2>/dev/null || die "Server exited, changed identity, or resource sampling failed"
        mapfile -t active_jobs < <(jobs -pr)
        inflight=${#active_jobs[@]}
        if ((inflight - 1 >= limit)); then
            ((dropped+=1))
            continue
        fi
        idx=$((seq % count))
        local id path url flags unused
        IFS=$'\t' read -r id path url flags unused <<< "${rows[$idx]}"
        request "$seq" "$id" "$path" "$url" "$flags" "$scheduled" &
    done
    # Include the full offered-load window, then drain outstanding requests.
    current=$(now)
    delay=$(awk -v s="$start" -v d="$duration" -v t="$current" 'BEGIN {v=d-(t-s)/1e6;printf "%.6f",(v>0?v:0)}')
    sleep "$delay"
    local failed=0
    for child in $(jobs -pr); do
        [[ $child == "$sampler" ]] || { wait "$child" || failed=1; }
    done
    kill -0 "$sampler" 2>/dev/null || die "Resource sampling stopped unexpectedly"
    kill "$sampler"
    wait "$sampler" 2>/dev/null || :
    ((failed == 0)) || die "A request worker failed"
    finish=$(now)
    local elapsed
    elapsed=$(awk -v a="$finish" -v b="$start" 'BEGIN {printf "%.6f",(a-b)/1e6}')
    printf 'seq\tid\tcurl_exit\tsend_lag_ms\tstatus\toutput_bytes\tttfb_s\tlatency_s\tcontent_type\tcompleted_at_s\n' > "$out/requests.tsv"
    for ((seq=0; seq<total; seq++)); do
        [[ ! -f $out/requests/$seq.tsv ]] || cat "$out/requests/$seq.tsv" >> "$out/requests.tsv"
    done
    [[ $(($(wc -l < "$out/requests.tsv") - 1)) == "$((total - dropped))" ]] \
        || die "Missing request results; measurement incomplete"
    [[ $(wc -l < "$out/requests.tsv") -gt 1 ]] || die "No requests completed"
    summarize "$out" "$elapsed" "$hz"
    local lag_budget
    lag_budget=$(awk -v r="$rps" 'BEGIN {v=1000/r;print (v>100?v:100)}')
    jq --argjson dropped "$dropped" --argjson elapsed "$elapsed" --argjson budget "$lag_budget" \
        '.metrics.dropped_before_send=$dropped | .metrics.elapsed_including_drain_s=$elapsed |
         .metrics.send_lag_budget_ms=$budget |
         .valid_load=($dropped==0 and .metrics.send_lag_max_ms<=$budget)' "$out/summary.json" > "$out/summary.tmp"
    mv -- "$out/summary.tmp" "$out/summary.json"
    printf 'Saved %s/summary.json (unsent: %s)\n' "$out" "$dropped"
    ((dropped == 0)) || die "Load invalid: concurrency cap caused unsent requests"
    jq -e '.valid_load' "$out/summary.json" >/dev/null || die "Load invalid: sender lag exceeded budget"
}

[[ ${BASH_VERSINFO[0]} -ge 5 ]] || die "Bash 5 or later required"
for cmd in curl awk jq sha256sum; do need "$cmd"; done
if [[ $# == 0 || ${1:-} == --help || ${1:-} == -h ]]; then
    usage
    exit 0
fi
run "$@"
