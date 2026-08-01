#!/usr/bin/env bash
# Cargo runner: 构建完成后自动将产物复制为 .elf 并烧录。
#
# 由 .cargo/config.toml 的 [target.*].runner 调用,
# 第一个参数为 cargo 构建产物路径 (无扩展名)。
#
# 用法:
#   cargo run # 等价于: cargo run --release (构建 + 烧录)
#
# 可选环境变量:
#   PYOCD_PROBE   调试器 ID (等价于 pyocd 的 -u 参数, 缺省时自动选择)
#   PYOCD_TARGET  目标芯片型号 (缺省 hc32f460xe)
#   PYOCD         自定义 pyocd 可执行文件路径
#   FLASH_DRY_RUN 非空时只打印命令不真正烧录 (用于验证流程)
set -euo pipefail

# cargo 构建产物 (如 target/thumbv7em-none-eabi/release/hc32f460)
ELF="${1:?缺少构建产物路径}"
ELF_ELF="${ELF}.elf"

# 复制为 .elf 后缀 (pyocd 依赖扩展名/魔数识别格式, .elf 最明确)
cp "${ELF}" "${ELF_ELF}"

# 定位 pyocd (优先环境变量, 其次项目虚拟环境, 最后系统 PATH)
PYOCD_BIN="${PYOCD:-}"
if [[ -z "${PYOCD_BIN}" ]]; then
    if [[ -x ".venv/bin/pyocd" ]]; then
        PYOCD_BIN=".venv/bin/pyocd"
    elif command -v pyocd >/dev/null 2>&1; then
        PYOCD_BIN="pyocd"
    else
        echo "错误: 找不到 pyocd, 请通过 PYOCD 环境变量指定路径或安装依赖" >&2
        exit 1
    fi
fi

PROBE_ARGS=()
if [[ -n "${PYOCD_PROBE:-}" ]]; then
    PROBE_ARGS+=("-u" "${PYOCD_PROBE}")
fi
TARGET="${PYOCD_TARGET:-hc32f460xe}"

if [[ -n "${FLASH_DRY_RUN:-}" ]]; then
    echo "[dry-run] ${PYOCD_BIN} flash ${PROBE_ARGS[*]+"${PROBE_ARGS[@]}"} --target ${TARGET} ${ELF_ELF}"
    exit 0
fi

echo "==> 烧录 ${ELF_ELF}"
"${PYOCD_BIN}" flash "${PROBE_ARGS[@]}" --target "${TARGET}" "${ELF_ELF}"
