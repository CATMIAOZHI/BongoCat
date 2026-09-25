# 自建中继：多阶段构建，产物只有一个静态二进制 + 一个非 root 用户。
#
# 只需要 Docker；TLS 由前置的 Caddy 终结，所以镜像里不需要证书与 Node。

FROM rust:1.97-slim-bookworm AS builder

WORKDIR /build

# 先只拷清单，让依赖层可以被 Docker 缓存（改代码不必重下依赖）
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --locked

FROM debian:bookworm-slim

RUN useradd --create-home --uid 10001 relay

COPY --from=builder /build/target/release/bongocat-pair-relay /usr/local/bin/bongocat-pair-relay

USER relay

ENV PAIR_LISTEN=0.0.0.0:8080

EXPOSE 8080
EXPOSE 3479/udp

ENTRYPOINT ["bongocat-pair-relay"]
