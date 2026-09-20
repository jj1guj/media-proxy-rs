#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="${LIBVIPS_VERSION:-8.18.0}"
target_dir="${CARGO_TARGET_DIR:-${repo_root}/target}"
source_dir="${target_dir}/libvips-src-${version}"
build_dir="${target_dir}/libvips-build-${version}"
prefix="${LIBVIPS_PREFIX:-${target_dir}/libvips}"

case "$(uname -s)" in
    Darwin) library_link_name="libvips.dylib" ;;
    Linux) library_link_name="libvips.so" ;;
    *)
        echo "error: only macOS and Linux are supported" >&2
        exit 1
        ;;
esac

fix_macos_install_name() {
    if [[ "$(uname -s)" != "Darwin" ]]; then
        return
    fi
    local dylib
    dylib="$(find "${prefix}/lib" -maxdepth 1 -type f -name 'libvips*.dylib' -print -quit)"
    if [[ -n "${dylib}" ]]; then
        install_name_tool -id "${dylib}" "${dylib}"
    fi
}

if [[ -e "${prefix}/lib/${library_link_name}" ]]; then
    fix_macos_install_name
    echo "minimal libvips already exists in ${prefix}"
    exit 0
fi

for command in git meson ninja pkg-config python3; do
    if ! command -v "${command}" >/dev/null 2>&1; then
        echo "error: ${command} is required" >&2
        exit 1
    fi
done

if ! pkg-config --exists glib-2.0 gobject-2.0; then
    echo "error: GLib development files are required (for example: brew install glib or apt install libglib2.0-dev)" >&2
    exit 1
fi

mkdir -p "$(dirname "${source_dir}")" "$(dirname "${build_dir}")"

if [[ ! -d "${source_dir}/.git" ]]; then
    git clone --branch "v${version}" --depth 1 \
        https://github.com/libvips/libvips.git "${source_dir}"
fi

setup_args=(
    "${build_dir}"
    "${source_dir}"
    "--prefix=${prefix}"
    "--libdir=lib"
    "--buildtype=release"
    "--auto-features=disabled"
    "-Ddeprecated=false"
    "-Dexamples=false"
    "-Dcplusplus=false"
    "-Dcpp-docs=false"
    "-Ddocs=false"
    "-Dmodules=disabled"
    "-Dintrospection=disabled"
    "-Dvapi=false"
    "-Dnsgif=false"
    "-Dppm=false"
    "-Danalyze=false"
    "-Dradiance=false"
    "-Dfuzzing_engine=none"
)

if [[ -f "${build_dir}/build.ninja" ]]; then
    meson setup --reconfigure "${setup_args[@]}"
else
    meson setup "${setup_args[@]}"
fi

library_path="$(
    meson introspect --targets "${build_dir}" | python3 -c '
import json
import sys

for target in json.load(sys.stdin):
    if target["name"] == "vips" and target["type"] == "shared library":
        print(target["filename"][0])
        break
'
)"

if [[ -z "${library_path}" ]]; then
    echo "error: the libvips shared-library target was not found" >&2
    exit 1
fi

ninja -C "${build_dir}" "${library_path#"${build_dir}/"}"

if [[ ! -f "${library_path}" ]]; then
    echo "error: the built libvips shared library was not found" >&2
    exit 1
fi

if [[ -z "${prefix}" || "${prefix}" == "/" ]]; then
    echo "error: refusing to replace an unsafe LIBVIPS_PREFIX" >&2
    exit 1
fi

rm -rf "${prefix}"
mkdir -p "${prefix}/lib"
cp "${library_path}" "${prefix}/lib/"

library_name="$(basename "${library_path}")"
if [[ "${library_name}" != "${library_link_name}" ]]; then
    if [[ "${library_link_name}" == "libvips.so" && "${library_name}" =~ ^libvips\.so\.([0-9]+) ]]; then
        soname="libvips.so.${BASH_REMATCH[1]}"
        if [[ "${soname}" != "${library_name}" ]]; then
            ln -s "${library_name}" "${prefix}/lib/${soname}"
        fi
        ln -s "${soname}" "${prefix}/lib/${library_link_name}"
    else
        ln -s "${library_name}" "${prefix}/lib/${library_link_name}"
    fi
fi

fix_macos_install_name
rm -rf "${source_dir}" "${build_dir}"

echo "minimal libvips ${version} installed in ${prefix}"
