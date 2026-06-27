FROM --platform=$BUILDPLATFORM public.ecr.aws/docker/library/rust:latest AS cross_build
ARG BUILDARCH
ARG TARGETARCH
ARG TARGETVARIANT
RUN apt-get update && apt-get install -y clang musl-dev pkg-config nasm mold git meson ninja-build xz-utils cmake
COPY crossfiles /app/crossfiles
RUN bash /app/crossfiles/deps.sh

FROM --platform=$BUILDPLATFORM cross_build AS dav1d
RUN git clone --branch 1.4.3 --depth 1 https://github.com/videolan/dav1d.git /dav1d_src
RUN cd /dav1d_src && bash -c "source /app/crossfiles/meson.sh && meson build -Dprefix=/dav1d -Denable_tools=false -Denable_examples=false -Ddefault_library=static --buildtype release --cross-file /app/crossfiles/cross.txt"
RUN cd /dav1d_src && bash -c "source /app/crossfiles/meson.sh && ninja -C build"
RUN cd /dav1d_src && bash -c "source /app/crossfiles/meson.sh && ninja -C build install"

FROM --platform=$BUILDPLATFORM cross_build AS lcms2
RUN git clone -b lcms2.16 --depth 1 https://github.com/mm2/Little-CMS.git /lcms2_src
RUN cd /lcms2_src && bash -c "source /app/crossfiles/meson.sh && meson build --prefix=/lcms2 -Ddefault_library=static -Dfastfloat=true -Dthreaded=true --buildtype release --cross-file /app/crossfiles/cross.txt"
RUN cd /lcms2_src && bash -c "source /app/crossfiles/meson.sh && ninja -C build"
RUN cd /lcms2_src && bash -c "source /app/crossfiles/meson.sh && ninja -C build install"

FROM --platform=$BUILDPLATFORM cross_build AS build_app
ENV CARGO_HOME=/var/cache/cargo
ENV SYSTEM_DEPS_LINK=static
ENV TURBOJPEG_SOURCE=vendor
ENV PKG_CONFIG_ALLOW_CROSS=1
ENV PKG_CONFIG_LIBDIR=/dav1d/lib/pkgconfig:/lcms2/lib/pkgconfig
ENV PKG_CONFIG_PATH=/dav1d/lib/pkgconfig:/lcms2/lib/pkgconfig
WORKDIR /app
COPY avif-decoder_dep ./avif-decoder_dep
COPY .gitmodules ./.gitmodules
COPY --from=dav1d /dav1d /dav1d
COPY --from=lcms2 /lcms2 /lcms2
ENV LD_LIBRARY_PATH=/dav1d/lib:/lcms2/lib
COPY src ./src
COPY Cargo.toml ./Cargo.toml
COPY asset ./asset
COPY examples ./examples
RUN --mount=type=cache,target=/var/cache/cargo --mount=type=cache,target=/app/target bash /app/crossfiles/build.sh

FROM public.ecr.aws/docker/library/alpine:latest
ARG UID="852"
ARG GID="852"
RUN addgroup -g "${GID}" proxy && adduser -u "${UID}" -G proxy -D -h /media-proxy-rs -s /bin/sh proxy
WORKDIR /media-proxy-rs
USER proxy
COPY --from=build_app /app/media-proxy-rs ./media-proxy-rs
COPY --from=build_app /app/healthcheck ./healthcheck
RUN sh -c "./media-proxy-rs&" && ./healthcheck 12887 http://127.0.0.1:12766/test.webp
HEALTHCHECK --interval=30s --timeout=3s CMD ./healthcheck 5555 http://127.0.0.1:12766/test.webp || exit 1
EXPOSE 12766
CMD ["./media-proxy-rs"]
