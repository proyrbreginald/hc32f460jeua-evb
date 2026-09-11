#!/usr/bin/env bash
# Cargo runner: 构建完成后自动将产物复制为 .elf 并烧录。
#
# 由 .cargo/config.toml 的 [target.*].runner 调用,
# 第一个参数为 cargo 构建产物路径 (无扩展名)。
#
# 用法:
#   cargo run           # 构建 (debug) + 烧录
#   cargo run --release # 构建 release + 烧录
#
# 可选环境变量:
#   PYOCD_PROBE        调试器 ID (pyocd -u; 缺省自动选择)
#   PYOCD_TARGET       目标芯片型号 (缺省 hc32f460xe)
#   PYOCD              自定义 pyocd 可执行文件路径
#   FLASH_DRY_RUN      非空时只打印命令, 不真正烧录
#   FLASH_FREQ         SWD 时钟 (缺省沿用 pyocd 的 1MHz; 接线长/干扰大用 500k/250k)
#   FLASH_CONNECT      pyocd connect_mode: halt|pre-reset|under-reset|attach
#                      (缺省 halt; 目标固件配了 MPU/SWD 复用或被看门狗反复复位时
#                       用 under-reset —— 需调试器 nRST 已接到板上)
#   FLASH_RESET_TYPE   pyocd reset_type (default|hardware|system|core|...)
#   FLASH_MASS_ERASE   非空时整片擦除 (-e chip)
#   FLASH_EXTRA        追加给 pyocd flash 的额外参数 (按空白拆分)
#   FLASH_AUTO_RETRY   缺省 1: 首次失败后用 500kHz + under-reset 自动重试一次
#
# 烧录失败 ("cannot read register ipsr because core #0 is not halted" 等) 时,
# 脚本会打印排查阶梯; 原理见 docs/DEVELOPMENT.md 的"烧录与调试故障"一节。
set -euo pipefail

# cargo 构建产物 (如 target/thumbv7em-none-eabihf/release/hc32f460)
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

TARGET="${PYOCD_TARGET:-hc32f460xe}"

# 构建配置提示 (按产物路径判断, 不再固定打印 debug 文案)
if [[ "${ELF}" == *"/release/"* ]]; then
    PROFILE_NOTE="release 构建"
else
    PROFILE_NOTE="debug 构建"
fi

# 组装一次 pyocd 调用的完整参数 (子命令之后才是 -u 等选项)
ARGS=()
build_args() {
    local freq="${1:-}"
    local connect="${2:-}"
    ARGS=(flash)
    if [[ -n "${PYOCD_PROBE:-}" ]]; then
        ARGS+=("-u" "${PYOCD_PROBE}")
    fi
    ARGS+=("--target" "${TARGET}")
    if [[ -n "${freq}" ]]; then
        ARGS+=("-f" "${freq}")
    fi
    if [[ -n "${connect}" ]]; then
        ARGS+=("-O" "connect_mode=${connect}")
    fi
    if [[ -n "${FLASH_RESET_TYPE:-}" ]]; then
        ARGS+=("-O" "reset_type=${FLASH_RESET_TYPE}")
    fi
    if [[ -n "${FLASH_MASS_ERASE:-}" ]]; then
        ARGS+=("-e" "chip")
    fi
    if [[ -n "${FLASH_EXTRA:-}" ]]; then
        # shellcheck disable=SC2206 # 有意按空白拆分用户提供的额外参数
        ARGS+=(${FLASH_EXTRA})
    fi
    ARGS+=("${ELF_ELF}")
}

print_ladder() {
    cat >&2 <<'EOF'

==> 烧录失败。若报错为 "cannot read register ipsr ... is not halted", 说明 pyocd
    在擦除/编程期间失去了对内核的控制: 目标被复位 (掉电/看门狗/nRST 被拉低),
    或 SWD 链路不稳。按下面顺序排查 (用环境变量即可, 不必改脚本):

  1) SWD 降速 (最常见的接线/干扰问题):
       FLASH_FREQ=250k cargo run --release
  2) 复位下连接 (需调试器 nRST 接到板子; 可绕开已运行固件配的 MPU/看门狗):
       FLASH_CONNECT=under-reset cargo run --release
  3) 整片擦除 (清理被保护或半擦除的 Flash):
       FLASH_MASS_ERASE=1 cargo run --release
  4) 用板子自己的电源供电 (别用调试器 3.3V 带载), 确认共地: erase 电流尖峰
     导致的欠压会直接触发本错误;
  5) 读寄存器判断当时状态:
       .venv/bin/pyocd commander -t hc32f460xe -N -c "read32 0xE000EDF0"  # DHCSR
       .venv/bin/pyocd commander -t hc32f460xe -c "read32 0xE000ED90"     # MPU_CTRL
  6) 排除固件干扰: 目标上已运行的固件若使能了看门狗 (MCU 内部 CFG_WDT_ENABLE 或
     板载外部 CFG_HWDT_ENABLE), 调试器停机即停止喂狗, 擦除中途必被复位。用
     CFG_WDT_ENABLE=false / CFG_HWDT_ENABLE=false / CFG_MPU_ENABLE=false 各烧一次
     做对照 (原理见 docs/DEVELOPMENT.md 的"烧录与调试故障")。
EOF
}

run_flash() {
    local freq="$1"
    local connect="$2"
    local label="$3"
    build_args "${freq}" "${connect}"
    echo "==> 烧录 ${ELF_ELF} (${PROFILE_NOTE}${label})"
    echo "    ${PYOCD_BIN} ${ARGS[*]}"
    if [[ -n "${FLASH_DRY_RUN:-}" ]]; then
        return 0
    fi
    "${PYOCD_BIN}" "${ARGS[@]}"
}

if [[ -n "${FLASH_DRY_RUN:-}" ]]; then
    run_flash "${FLASH_FREQ:-}" "${FLASH_CONNECT:-}" ""
    exit 0
fi

if run_flash "${FLASH_FREQ:-}" "${FLASH_CONNECT:-}" ""; then
    exit 0
fi

# 首次失败: 用户未显式指定频率/连接方式时, 用更保守的参数自动重试一次
if [[ "${FLASH_AUTO_RETRY:-1}" != "0" && -z "${FLASH_FREQ:-}" && -z "${FLASH_CONNECT:-}" ]]; then
    echo "==> 首次烧录失败, 自动重试: 500kHz SWD + 复位下连接 (under-reset)" >&2
    if run_flash "500k" "under-reset" " — 自动重试: 500kHz + under-reset"; then
        exit 0
    fi
fi

print_ladder
exit 1
