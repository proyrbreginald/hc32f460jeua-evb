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

## Python 虚拟环境与 pyOCD

### 一、创建 Python 虚拟环境

```bash
python3 -m venv .venv
source .venv/bin/activate
```

### 二、安装 pyOCD

在虚拟环境中安装 `pyocd`:

```bash
pip install pyocd
```

### 三、连接调试器与检测目标

将调试器（如 DAPLink、ST-Link、J-Link）通过 SWD 接口连接到目标板，然后检测芯片：

```bash
pyocd list # 列出可用调试器
```

### 四、烧录固件

```bash
pyocd flash -u <调试器ID> --target hc32f460xe target/thumbv7em-none-eabihf/debug/hc32f460.elf
```

参数说明：
- `--target hc32f460xe`  指定目标芯片型号
- `--base-address 0x00000000`  可选，指定烧录起始地址
- `--erase auto`  可选，自动选择擦除策略（sector / chip）

### 五、调试

#### 启动 GDB 服务器

```bash
pyocd gdbserver --target hc32f460xe
```

默认监听端口 `3333`，可用 `--port` 指定其他端口。

#### 连接 GDB

在另一个终端中启动 GDB 并连接：

```bash
arm-none-eabi-gdb -q target/thumbv7em-none-eabihf/debug/hc32f460.elf
target extended-remote localhost:3333
monitor reset halt   # 复位并暂停
load                 # 烧录当前 ELF
continue             # 运行
```

### 六、退出虚拟环境

```bash
deactivate
```