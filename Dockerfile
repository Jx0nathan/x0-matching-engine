# 撮合引擎服务镜像
#
# 构建：docker build -t matching-server .
# 运行：docker run -d --name matching \
#         -p 8080:8080 \
#         -v matching-data:/data \
#         --stop-timeout 120 \
#         matching-server
#
# --stop-timeout 很重要：默认只给 10 秒。停机要排空在途命令并写最终快照，
# 超时会被 SIGKILL，届时在途命令与整个内存状态一起丢失。

# ---------- 构建阶段 ----------
FROM rust:1-slim-bookworm AS builder

WORKDIR /build
COPY . .

# BuildKit 缓存挂载：依赖与中间产物跨次构建复用。
# 不用"先拷 Cargo.toml 造空壳"那套技巧 —— 本项目声明了 bench/example 目标，
# 空壳会因目标文件缺失而无法解析清单。
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --bin matching-server \
 && cp target/release/matching-server /usr/local/bin/matching-server

# ---------- 运行阶段 ----------
FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*

# 非 root 运行。数据目录要先建好并授权，否则挂卷后属主是 root，进程写不进去。
RUN useradd --system --uid 10001 --create-home --home-dir /var/lib/matching matching \
 && mkdir -p /data \
 && chown matching:matching /data

COPY --from=builder /usr/local/bin/matching-server /usr/local/bin/matching-server

USER matching
WORKDIR /var/lib/matching

ENV BIND=0.0.0.0:8080 \
    DATA_DIR=/data \
    WAL_SYNC=64 \
    SYMBOLS=1:0:1 \
    RUST_LOG=info

# WAL 与快照都在这里，容器重建后必须挂回同一个卷才能恢复状态
VOLUME ["/data"]
EXPOSE 8080

HEALTHCHECK --interval=10s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -fsS http://127.0.0.1:8080/health || exit 1

# exec 形式：进程即 PID 1，能直接收到 SIGTERM。
# 服务内部显式注册了 SIGTERM 处理（PID 1 没有默认处理器，必须显式注册）。
ENTRYPOINT ["matching-server"]
