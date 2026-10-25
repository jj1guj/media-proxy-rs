#!/usr/bin/env bash
set -euo pipefail
[[ $# == 1 ]] || { printf 'Usage: bash %s DATASET_DIR\n' "$0" >&2; exit 1; }
root=$(cd -- "$1" && pwd)
[[ -f $root/SHA256SUMS && -f $root/httpd.conf && -f $root/dataset.json ]] \
    || { printf 'ERROR: incomplete dataset\n' >&2; exit 1; }
command -v busybox >/dev/null || { printf 'ERROR: busybox required\n' >&2; exit 1; }
(cd -- "$root" && sha256sum --quiet -c SHA256SUMS)
printf 'Serving fixed images at http://127.0.0.1:18080 (Ctrl-C to stop)\n' >&2
exec busybox httpd -f -vv -p 127.0.0.1:18080 -h "$root/images" -c "$root/httpd.conf"
