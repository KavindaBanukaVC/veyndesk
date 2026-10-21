# Veyndesk server — single static binary, ~15 MB image.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p veyndesk-server

FROM debian:bookworm-slim
RUN useradd -r -u 10001 veyndesk && mkdir /data && chown veyndesk /data
COPY --from=build /src/target/release/veyndesk-server /usr/local/bin/veyndesk-server
USER veyndesk
ENV VEYNDESK_BIND=0.0.0.0:8080 VEYNDESK_DATA=/data
VOLUME /data
EXPOSE 8080
HEALTHCHECK CMD ["/bin/sh", "-c", "exec 3<>/dev/tcp/127.0.0.1/8080 || exit 1"]
ENTRYPOINT ["veyndesk-server"]
