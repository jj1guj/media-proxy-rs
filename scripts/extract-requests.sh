#!/usr/bin/env bash
set -euo pipefail
export LC_ALL=C

if [[ $# != 2 || ! -r $1 || -e $2 ]]; then
    printf 'Usage: bash %s LOG NEW_REQUESTS.tsv\n' "$0" >&2
    exit 1
fi

tmp=$(mktemp)
trap 'rm -f -- "$tmp"' EXIT
# Compose prefixes and tracing ANSI decoration are not part of the fields.
awk '
    BEGIN {OFS="\t"; print "id","path","url","flags"}
    {
        gsub(/\033\[[0-9;]*m/, "")
        if ($0 !~ /media_proxy_rs.* request /) next
        path=""; url=""; flags="-"
        for (i=1;i<=NF;i++) {
            if ($i ~ /^path=/) path=substr($i,6)
            if ($i ~ /^url=/) url=substr($i,5)
            if ($i ~ /^params=/ && length($i)>7) flags=substr($i,8)
        }
        if (url !~ /^https?:\/\//) {skipped++; next}
        if (path !~ /^\/[^?#]*$/) {
            print "Invalid request at log line " NR > "/dev/stderr"; bad++; next
        }
        if (flags !~ /^(-|((static|emoji|avatar|preview|badge|fallback),?)+)$/) {
            print "Unknown flags at log line " NR > "/dev/stderr"; bad++; next
        }
        print ++n,path,url,flags
    }
    END {
        if (skipped) print "WARNING: excluded " skipped " non-HTTP(S) image URLs" > "/dev/stderr"
        if (!n || bad) {
            print "Extraction failed: requests=" n+0 ", invalid=" bad+0 > "/dev/stderr"
            exit 1
        }
    }
' "$1" > "$tmp"
mv -T -- "$tmp" "$2"
printf 'Extracted %s requests to %s\n' "$(($(wc -l < "$2") - 1))" "$2"
