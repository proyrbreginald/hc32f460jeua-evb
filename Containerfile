# 通过 DaoCloud 镜像站拉取 Ubuntu 基础镜像，避免 Docker Hub 网络问题
FROM docker.m.daocloud.io/library/ubuntu:latest

# 让 apt 跳过所有交互提示，直接使用默认选项，确保构建不会因等待用户输入而卡死
ENV DEBIAN_FRONTEND=noninteractive

# 更新软件仓库列表
RUN apt update
RUN apt upgrade -y

# 安装终端浏览器
RUN apt install curl -y

# 安装 python3 环境
RUN apt install python3 python3-venv -y

# 安装 rust 环境
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

# 导入环境变量
ENV PATH="/root/.cargo/bin:${PATH}"