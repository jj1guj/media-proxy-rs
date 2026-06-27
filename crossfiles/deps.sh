set -eu
if [ -f "/app/crossfiles/${TARGETARCH}.sh" ]; then
	source /app/crossfiles/${TARGETARCH}.sh
else
	source /app/crossfiles/${TARGETARCH}/${TARGETVARIANT}.sh
fi
rustup target add ${RUST_TARGET}
mkdir /musl
curl -sSL https://github.com/userdocs/qbt-musl-cross-make/releases/download/2604/x86_64-${MUSL_NAME}.tar.xz | xz -d | tar -xf - -C /musl
mkdir -p /musl/${MUSL_NAME}/dav1d/
