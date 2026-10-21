# VeloDesk server — single static binary, ~15 MB image.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p velodesk-server

FROM debian:bookworm-slim
RUN useradd -r -u 10001 velodesk && mkdir /data && chown velodesk /data
COPY --from=build /src/target/release/velodesk-server /usr/local/bin/velodesk-server
USER velodesk
ENV VELODESK_BIND=0.0.0.0:8080 VELODESK_DATA=/data
VOLUME /data
EXPOSE 8080
HEALTHCHECK CMD ["/bin/sh", "-c", "exec 3<>/dev/tcp/127.0.0.1/8080 || exit 1"]
ENTRYPOINT ["velodesk-server"]
