set -eu
if [ -f "/app/crossfiles/${TARGETARCH}.sh" ]; then
	source /app/crossfiles/${TARGETARCH}.sh
else
	source /app/crossfiles/${TARGETARCH}/${TARGETVARIANT}.sh
fi
export RUSTFLAGS="${RUSTFLAGS} -C target-feature=-crt-static -C link-self-contained=no -L native=/vips-system-lib -L native=/vips/lib -C link-arg=-Wl,-rpath-link,/vips-system-lib -C link-arg=-Wl,-rpath-link,/vips/lib"
printf 'GROUP ( /vips-system-lib/libgcc_s.so.1 %s )\n' "$("${CC}" -print-libgcc-file-name)" > /vips-system-lib/libgcc_s.so
mkdir -p /musl/${MUSL_NAME}/dav1d /musl/${MUSL_NAME}/lcms2
cp -r /dav1d/lib /musl/${MUSL_NAME}/dav1d/lib
cp -r /lcms2/lib /musl/${MUSL_NAME}/lcms2/lib
mkdir ./.cargo/
echo "[target.${RUST_TARGET}]" >> ./.cargo/config.toml
echo 'rustflags = ["-C", "link-arg=-fuse-ld=/usr/bin/mold"]' >> ./.cargo/config.toml
cargo build --release --target ${RUST_TARGET}
cargo build --release --target ${RUST_TARGET} --example healthcheck
cp /app/target/${RUST_TARGET}/release/media-proxy-rs /app/media-proxy-rs
cp /app/target/${RUST_TARGET}/release/examples/healthcheck /app/healthcheck
