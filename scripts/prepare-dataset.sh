#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C

die() { printf 'ERROR: %s\n' "$*" >&2; exit 1; }
[[ $# == 2 || $# == 3 ]] || die "Usage: bash $0 REQUESTS.tsv DATASET_DIR [URL_COUNT]"
input=$1
out=${2%/}
count=${3:-1000}
[[ -r $input && $count =~ ^[1-9][0-9]*$ ]] \
    || die "Input must exist, URL_COUNT must be positive"
for cmd in curl awk jq sha256sum flock xargs nproc cmp; do
    command -v "$cmd" >/dev/null || die "Required command: $cmd"
done
awk -F '\t' '
    NR==1 {
        if ($0!="id\tpath\turl\tflags" &&
            $0!="id\tpath\turl\tflags\tsource_bytes\tsource_sha256") exit 1
        next
    }
    (NF!=4 && NF!=6) || $1!=NR-1 || $2!~/^\/[^?#]*$/ ||
        $3!~/^https?:\/\// || $4!~/^(-|((static|emoji|avatar|preview|badge|fallback),?)+)$/ {exit 1}
    END {if(NR<2)exit 1}
' "$input" || die "Invalid request manifest"
input_hash=$(sha256sum "$input"); input_hash=${input_hash%% *}
if [[ -e $out ]]; then
    [[ -d $out && -f $out/input.sha256 && -f $out/selected.tsv ]] \
        || die "Existing output is not a resumable dataset"
    [[ $(< "$out/input.sha256") == "$input_hash" ]] \
        || die "Input differs from existing dataset"
fi
mkdir -p -- "$out/images" "$out/work"
out=$(cd -- "$out" && pwd)
exec {dataset_lock}> "$out/.prepare.lock"
flock -n "$dataset_lock" || die "Dataset preparation is already running"
trap 'printf "ERROR: dataset preparation incomplete: %s\n" "$out" >&2' ERR
trap 'exit 130' INT
trap 'exit 143' TERM

# Select evenly spaced unique URLs in first-occurrence order, without randomness.
awk -F '\t' 'BEGIN {OFS="\t"} NR>1 && !seen[$3]++ {print $1,$2,$3,$4}' "$input" > "$out/work/unique.tsv"
unique=$(wc -l < "$out/work/unique.tsv")
((count<=unique)) || die "Not enough unique URLs for $count successful downloads"
awk -F '\t' -v n="$unique" -v count="$count" '
    BEGIN {for(i=0;i<count;i++) selected[int(i*n/count)+1]=1}
    NR in selected {print}
' "$out/work/unique.tsv" > "$out/work/selected.tsv"
if [[ -f $out/selected.tsv ]]; then
    head -n "$count" "$out/selected.tsv" > "$out/work/initial.tsv"
    cmp -s "$out/work/initial.tsv" "$out/work/selected.tsv" \
        || die "URL selection differs; use the original URL_COUNT"
else
    cp -- "$out/work/selected.tsv" "$out/selected.tsv"
fi
printf '%s\n' "$input_hash" > "$out/input.sha256"
if [[ -f $out/images.tsv ]]; then
    while IFS=$'\t' read -r id type size hash; do
        [[ -f $out/work/$id.ok ]] || printf '%s\t%s\t%s\t%s\n' \
            "$id" "$type" "$size" "$hash" > "$out/work/$id.ok"
    done < "$out/images.tsv"
fi
if [[ -f $out/excluded.tsv ]]; then
    while IFS=$'\t' read -r id code rc type; do
        [[ -f $out/work/$id.failed || -f $out/work/$id.ok ]] || \
            printf '%s\t%s\t%s\t%s\n' "$id" "$code" "$rc" "$type" > "$out/work/$id.failed"
    done < "$out/excluded.tsv"
fi
printf 'Target: %s successful downloads; reusing saved files; same host: one request at a time, one-second cooldown\n' "$count" >&2

download() {
    local out=$1 id=$2 url=$3 current=$3 code rc reply redirect host key fd
    local deadline delay retry_after now redirects=0 type hash size
    if [[ -f $out/work/$id.ok ]]; then
        local saved_id
        IFS=$'\t' read -r saved_id type size hash < "$out/work/$id.ok"
        [[ $saved_id == "$id" && -f $out/images/$id.img$id ]] \
            || { printf 'ERROR: saved file missing for request %s\n' "$id" >&2; return 1; }
        local actual_hash actual_size
        actual_hash=$(sha256sum "$out/images/$id.img$id"); actual_hash=${actual_hash%% *}
        actual_size=$(wc -c < "$out/images/$id.img$id")
        [[ $actual_hash == "$hash" && $actual_size == "$size" ]] \
            || { printf 'ERROR: saved file changed for request %s\n' "$id" >&2; return 1; }
        rm -f -- "$out/work/$id.failed"
        return
    fi
    rm -f -- "$out/work/$id.failed"
    while :; do
        host=${current#*://}; host=${host%%[/?#]*}; host=${host##*@}
        if [[ $host == \[* ]]; then host=${host%%]*}]; else host=${host%%:*}; fi
        host=${host,,}; host=${host%.}
        key=$(printf '%s' "$host" | sha256sum); key=${key%% *}
        exec {fd}> "$out/work/$key.lock"
        flock "$fd"
        deadline=0
        [[ ! -f $out/work/$key.next ]] || read -r deadline < "$out/work/$key.next"
        delay=$(awk -v d="$deadline" -v t="$EPOCHREALTIME" 'BEGIN{printf "%.6f",(d>t?d-t:0)}')
        if awk -v d="$delay" 'BEGIN{exit !(d>1)}'; then
            exec {fd}>&-
            printf 'WARNING: request %s deferred: host cooldown %ss; using another URL\n' "$id" "$delay" >&2
            printf '%s\t429\t0\t-\n' "$id" > "$out/work/$id.failed"
            return
        fi
        sleep "$delay"
        rc=0
        reply=$(curl --silent --show-error --globoff --proto '=http,https' \
            --connect-timeout 10 --max-time 60 --max-filesize 33554432 \
            --dump-header "$out/work/$id.headers" --output "$out/work/$id.part" \
            --write-out $'%{http_code}\n%{redirect_url}\n%{content_type}' -- "$current") || rc=$?
        code=${reply%%$'\n'*}; reply=${reply#*$'\n'}
        redirect=${reply%%$'\n'*}; type=${reply#*$'\n'}; type=${type%%;*}
        now=$EPOCHREALTIME
        delay=1
        if [[ $code == 429 ]]; then
            retry_after=$(awk '
                /^HTTP\// {v=""}
                tolower($0)~/^retry-after:/ {sub(/^[^:]*:[ \t]*/,"");sub(/\r$/,"");v=$0}
                END{print v}
            ' "$out/work/$id.headers")
            if [[ $retry_after =~ ^[0-9]+$ ]]; then
                delay=$retry_after
            elif [[ -n $retry_after ]] && deadline=$(date -d "$retry_after" +%s 2>/dev/null); then
                delay=$(awk -v d="$deadline" -v t="$now" 'BEGIN{print d-t}')
            else
                delay=60
            fi
            printf 'WARNING: request %s: 429, respecting host cooldown\n' "$id" >&2
        fi
        awk -v t="$now" -v d="$delay" 'BEGIN{printf "%.6f\n",t+(d>1?d:1)}' > "$out/work/$key.next"
        exec {fd}>&-
        case $rc in
            1|2|3|4|23|26|27|43|48)
                printf 'ERROR: request %s: local curl failure %s\n' "$id" "$rc" >&2
                return 1 ;;
        esac
        if [[ $rc == 0 && $code =~ ^30[12378]$ && -n $redirect ]]; then
            ((redirects+=1))
            if ((redirects>5)); then rc=47; break; fi
            [[ $redirect == http://* || $redirect == https://* ]] || { rc=1; break; }
            current=$redirect
            continue
        fi
        break
    done
    if [[ $rc == 0 && $code == 200 && $type =~ ^[a-zA-Z0-9.+-]+/[a-zA-Z0-9.+-]+$ && -s $out/work/$id.part ]]; then
        mv -- "$out/work/$id.part" "$out/images/$id.img$id"
        size=$(wc -c < "$out/images/$id.img$id")
        hash=$(sha256sum "$out/images/$id.img$id"); hash=${hash%% *}
        printf '%s\t%s\t%s\t%s\n' "$id" "$type" "$size" "$hash" > "$out/work/$id.ok"
    else
        printf 'WARNING: request %s excluded: HTTP %s, curl %s, type %s\n' "$id" "$code" "$rc" "$type" >&2
        printf '%s\t%s\t%s\t%s\n' "$id" "$code" "$rc" "$type" > "$out/work/$id.failed"
    fi
    rm -f -- "$out/work/$id.part" "$out/work/$id.headers"
}
export -f download

find "$out/work" -name '*.ok' -exec awk -F '\t' '{print $1}' {} + > "$out/work/saved-ids"
saved=$(wc -l < "$out/work/saved-ids")
((saved<=count)) || die "Existing dataset exceeds requested count"
awk -F '\t' -v needed="$((count-saved))" '
    FILENAME==ARGV[1]{saved[$1]=1;next}
    $1 in saved {print;next}
    n<needed {print;n++}
' "$out/work/saved-ids" "$out/selected.tsv" > "$out/work/batch.tsv"
while :; do
find "$out/work" -name '*.urls' -type f -delete
awk -F '\t' -v out="$out" '
    {
        host=$3;sub(/^https?:\/\//,"",host);sub(/[\/?#].*$/,"",host);host=tolower(host)
        if(!(host in group))group[host]=++n
        file=out "/work/" group[host] ".urls"
        print $1 "\t" $3 >> file;close(file)
    }
' "$out/work/batch.tsv"
find "$out/work" -name '*.urls' -type f -print0 |
    xargs -0 -r -n 1 -P "$(nproc)" bash -euo pipefail -c '
        while IFS=$'"'"'\t'"'"' read -r id url; do download "$1" "$id" "$url"; done < "$2"
    ' _ "$out"
find "$out/work" -name '*.ok' -exec cat {} + | sort -t $'\t' -k1,1n > "$out/images.tsv"
find "$out/work" -name '*.failed' -exec cat {} + | sort -t $'\t' -k1,1n > "$out/excluded.tsv"
successful=$(wc -l < "$out/images.tsv")
printf 'Progress: %s/%s saved\n' "$successful" "$count" >&2
((successful<count)) || break
awk -F '\t' -v needed="$((count-successful))" '
    FILENAME==ARGV[1]{seen[$1]=1;next}
    !($1 in seen) && n<needed {print;n++}
' "$out/selected.tsv" "$out/work/unique.tsv" > "$out/work/batch.tsv"
[[ -s $out/work/batch.tsv ]] || break
cat "$out/work/batch.tsv" >> "$out/selected.tsv"
done
[[ -s $out/images.tsv ]] || die "Only 0/$count saved: exhausted unique URLs; download results retained for resume"
awk -F '\t' '{print ".img" $1 ":" $2}' "$out/images.tsv" > "$out/httpd.conf"
awk -F '\t' '{print $4 "  images/" $1 ".img" $1}' "$out/images.tsv" > "$out/SHA256SUMS"
awk -F '\t' 'BEGIN{OFS="\t"}
    FILENAME==ARGV[1]{ok[$1]=1;next}
    FNR==1{print "id","path","url","flags"}
    $1 in ok{print ++n,$2,"http://127.0.0.1:18080/" $1 ".img" $1,$4}
' "$out/images.tsv" "$out/selected.tsv" > "$out/requests.tsv"
successful=$(wc -l < "$out/images.tsv")
attempted=$(wc -l < "$out/selected.tsv")
[[ $((successful + $(wc -l < "$out/excluded.tsv"))) == "$attempted" ]] \
    || die "Missing download results; dataset incomplete"
bytes=$(awk -F '\t' '{n+=$3}END{printf "%.0f",n}' "$out/images.tsv")
jq -n --argjson candidates "$attempted" --argjson target "$count" --argjson images "$successful" --argjson bytes "$bytes" \
    --arg sha "$(< "$out/input.sha256")" \
    '{input_sha256:$sha,selection:"evenly spaced unique URLs, replacements in first-occurrence order, first request per URL",target:$target,candidates:$candidates,images:$images,total_bytes:$bytes}' > "$out/dataset.json"
((successful==count)) || die "Only $successful/$count saved: exhausted unique URLs; saved files retained for resume"
find "$out/work" -type f -delete
rmdir "$out/work"
printf 'Saved %s response bodies; excluded %s. Requests: %s/requests.tsv\n' \
    "$successful" "$((attempted-successful))" "$out" >&2
