# 开发容器: Debian slim + Python (pyocd) + Rust (与 rust-toolchain.toml 同版本)。
# 容器内还需: python3 -m venv .venv && .venv/bin/pip install pyocd
# (见 README"开发环境容器")。
FROM debian:bookworm-slim

# 实际 cargo 家目录为 /root/.cargo (PATH 已指向)
ENV PATH="/root/.cargo/bin:${PATH}"

# 所有安装与清理步骤合并在同一条 RUN 指令中 (镜像层最小化)
RUN apt-get update && apt-get install -y --no-install-recommends \
    curl \
    python3 \
    python3-venv \
    && \
    # 安装 Rust: 固定版本 (与仓库 rust-toolchain.toml 一致, 可复现构建);
    # minimal profile + rustfmt/clippy (scripts/verify.sh 的前置条件)
    # + 目标双架构
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
       --no-modify-path \
       --profile minimal \
       --default-toolchain 1.98.1 \
       --component rustfmt \
       --component clippy \
       --target thumbv7em-none-eabihf \
       --target x86_64-unknown-linux-gnu \
    && \
    # 清理: 移除 curl、apt 列表与 cargo 缓存 (路径为 /root/.cargo)
    apt-get purge -y --auto-remove curl \
    && rm -rf /var/lib/apt/lists/* \
    && rm -rf /root/.cargo/registry/cache/* \
    && rm -rf /root/.cargo/registry/src/*
