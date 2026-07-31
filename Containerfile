# 1. 换成更轻量的基础镜像（debian:slim 只有约 30MB，且与 Ubuntu 一样使用 apt）
FROM debian:bookworm-slim

# 2. 设置环境变量
ENV PATH="/root/.cargo/bin:${PATH}"

# 3. 将所有安装与清理步骤合并在“同一条 RUN 指令”中
RUN apt-get update && apt-get install -y --no-install-recommends \
    curl \
    python3 \
    python3-venv \
    && \
    # 3. 安装 Rust 时指定 --profile minimal（只安装 rustc/cargo/std，不安装几百兆的文档）
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y \
       --no-modify-path \
       --profile minimal \
       --default-toolchain stable \
    && \
    # 4. 清理无用的编译依赖和缓存
    apt-get purge -y --auto-remove curl \
    && rm -rf /var/lib/apt/lists/* \
    && rm -rf /usr/local/cargo/registry/cache/* \
    && rm -rf /usr/local/cargo/git/db/*