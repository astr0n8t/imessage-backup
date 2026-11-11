FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p imsg-server

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends libheif-examples ffmpeg ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --home /state imsg && mkdir -p /state && chown imsg /state
COPY --from=build /src/target/release/imsg-server /usr/local/bin/imsg-server
USER imsg
ENV IMSG_DATA=/data IMSG_STATE_DIR=/state IMSG_AUTH_FILE=/accounts.toml IMSG_LISTEN=0.0.0.0:8080
EXPOSE 8080
ENTRYPOINT ["imsg-server"]
CMD ["serve"]
