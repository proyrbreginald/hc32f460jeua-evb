# hc32f460jeua-evb

## 开发环境容器

本项目的 `Containerfile` 定义了一个包含 **Python** 和 **Rust** 开发环境的 Ubuntu 容器镜像。

### 前置条件

确保已安装 Podman：

```bash
sudo apt install podman -y
```

### 一、构建镜像

```bash
podman build --no-cache -t <镜像名:版本号> -f <镜像构建脚本> .
```

### 二、创建容器

```bash
podman create -it -v "$PWD":/workspace --name <容器名称> <镜像名称>
```

### 三、运行容器

```bash
podman start <容器名称>
podman exec -it <容器名称> bash
```

### 四、停止容器

```bash
podman stop <容器名称>
```

### 五、删除容器与镜像

```bash
podman rm <容器名称>
podman rmi <镜像名称>
```