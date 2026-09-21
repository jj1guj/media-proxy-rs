#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
imagemagick_version="${IMAGEMAGICK_VERSION:-7.1.2-30}"
libpng_version="${LIBPNG_VERSION:-1.6.50}"
zlib_version="${ZLIB_VERSION:-1.3.1}"
target_dir="${CARGO_TARGET_DIR:-${repo_root}/target}"
imagemagick_source_dir="${target_dir}/imagemagick-src-${imagemagick_version}"
libpng_source_dir="${target_dir}/libpng-src-${libpng_version}"
zlib_source_dir="${target_dir}/zlib-src-${zlib_version}"
prefix="${IMAGEMAGICK_PREFIX:-${target_dir}/imagemagick}"
version_marker="${prefix}/.source-versions"
expected_versions="ImageMagick=${imagemagick_version}
libpng=${libpng_version}
zlib=${zlib_version}"

if [[ -f "${prefix}/lib/pkgconfig/MagickWand.pc" ]] \
    && [[ -f "${version_marker}" ]] \
    && [[ "$(cat "${version_marker}")" == "${expected_versions}" ]]; then
    echo "minimal ImageMagick already exists in ${prefix}"
    exit 0
fi

for command in git make pkg-config; do
    if ! command -v "${command}" >/dev/null 2>&1; then
        echo "error: ${command} is required" >&2
        exit 1
    fi
done

if [[ -z "${prefix}" || "${prefix}" == "/" ]]; then
    echo "error: refusing to replace an unsafe IMAGEMAGICK_PREFIX" >&2
    exit 1
fi

rm -rf "${prefix}" "${imagemagick_source_dir}" "${libpng_source_dir}" "${zlib_source_dir}"
mkdir -p "${prefix}" "${target_dir}"
jobs="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)"

git clone --branch "v${zlib_version}" --depth 1 \
    https://github.com/madler/zlib.git "${zlib_source_dir}"
cd "${zlib_source_dir}"
./configure --static --prefix="${prefix}"
make -j"${jobs}" install

git clone --branch "v${libpng_version}" --depth 1 \
    https://github.com/pnggroup/libpng.git "${libpng_source_dir}"
cd "${libpng_source_dir}"
PKG_CONFIG_PATH="${prefix}/lib/pkgconfig" \
CPPFLAGS="-I${prefix}/include" \
LDFLAGS="-L${prefix}/lib" \
    ./configure \
    --prefix="${prefix}" \
    --enable-static \
    --disable-shared
make -j"${jobs}" install

git clone --branch "${imagemagick_version}" --depth 1 \
    https://github.com/ImageMagick/ImageMagick.git "${imagemagick_source_dir}"
cd "${imagemagick_source_dir}"
PKG_CONFIG_PATH="${prefix}/lib/pkgconfig" \
CPPFLAGS="-I${prefix}/include" \
LDFLAGS="-L${prefix}/lib" \
    ./configure \
    --prefix="${prefix}" \
    --enable-static \
    --disable-shared \
    --disable-docs \
    --disable-modules \
    --disable-openmp \
    --without-x \
    --without-bzlib \
    --without-djvu \
    --without-fftw \
    --without-freetype \
    --without-gslib \
    --without-heic \
    --without-jbig \
    --without-jpeg \
    --without-jxl \
    --without-lcms \
    --without-lqr \
    --without-lzma \
    --without-openexr \
    --without-pango \
    --without-perl \
    --without-raqm \
    --without-raw \
    --without-rsvg \
    --without-tiff \
    --without-webp \
    --without-wmf \
    --without-xml \
    --without-zstd \
    --with-png=yes \
    --with-zlib=yes

make -j"${jobs}" install

printf '%s\n' "${expected_versions}" > "${version_marker}"
rm -rf "${imagemagick_source_dir}" "${libpng_source_dir}" "${zlib_source_dir}"
echo "minimal ImageMagick ${imagemagick_version} with libpng ${libpng_version} and zlib ${zlib_version} installed in ${prefix}"
