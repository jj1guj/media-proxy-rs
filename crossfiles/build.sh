set -eu
if [ -f "/app/crossfiles/${TARGETARCH}.sh" ]; then
	source /app/crossfiles/${TARGETARCH}.sh
else
	source /app/crossfiles/${TARGETARCH}/${TARGETVARIANT}.sh
fi
export RUSTFLAGS="${RUSTFLAGS} -C target-feature=-crt-static -C link-self-contained=no -L native=/vips-system-lib -L native=/vips/lib -C link-arg=-fuse-ld=mold -C link-arg=-Wl,-rpath-link,/vips-system-lib -C link-arg=-Wl,-rpath-link,/vips/lib -C link-arg=-Wl,--allow-shlib-undefined"
libgcc_path="$("${CC}" -print-libgcc-file-name)"
printf 'GROUP ( /vips-system-lib/libgcc_s.so.1 %s )\n' "${libgcc_path}" > /vips-system-lib/libgcc_s.so
printf 'GROUP ( /vips-system-lib/libgcc_s.so.1 %s )\n' "${libgcc_path}" > /vips-system-lib/libunwind.a
mkdir -p /musl/${MUSL_NAME}/dav1d /musl/${MUSL_NAME}/lcms2
cp -r /dav1d/lib /musl/${MUSL_NAME}/dav1d/lib
cp -r /lcms2/lib /musl/${MUSL_NAME}/lcms2/lib
cargo build --release --target ${RUST_TARGET}
cargo build --release --target ${RUST_TARGET} --example healthcheck
cp /app/target/${RUST_TARGET}/release/media-proxy-rs /app/media-proxy-rs
cp /app/target/${RUST_TARGET}/release/examples/healthcheck /app/healthcheck
