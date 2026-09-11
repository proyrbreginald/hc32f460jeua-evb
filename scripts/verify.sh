#!/usr/bin/env bash
# 本地全量验证: 主机测试 + 格式检查 + clippy + 目标构建 (默认/全开配置)。
#
# 设计动机 (见 README "代码整理与设计优化记录"):
#   - selftest/soak/nano 是 #[cfg] 门控模块, 默认 release 构建不编译它们,
#     签名/类型改动可能绕过检查 (曾出现 HEAD debug 构建失败);
#   - debug 构建恒开启自测功能 (build.rs dev_profile), release 按配置裁剪,
#     两者必须分别验证;
#   - 因此本脚本强制跑"全开配置"构建: 所有大功能编译期开启,
#     任何 cfg 门控代码都会被编译到。
#
# 用法:
#   scripts/verify.sh            # 全部检查
#   scripts/verify.sh --quick    # 只跑主机测试 + 目标 debug 构建
#
# 环境要求: rustup 目标 thumbv7em-none-eabihf; 可选 arm-none-eabi-size
# (位于 PATH 或 ARM_TOOLCHAIN 前缀) 用于体积报告。
set -euo pipefail

cd "$(dirname "$0")/.."

TARGET="thumbv7em-none-eabihf"
# 主机测试目标: 从工具链探测 (aarch64/macOS 等宿主机同样可用)
HOST_TARGET="$(rustc -vV | sed -n 's/^host: //p')"
QUICK="${1:-}"

step() { printf '\n==> %s\n' "$*"; }

# ---------------------------------------------------------------------------
# 1. 格式 (代码风格保持单一来源)
# ---------------------------------------------------------------------------
step "cargo fmt --check"
cargo fmt --all -- --check

# ---------------------------------------------------------------------------
# 2. 主机测试 (纯算法 + littlefs 断电穷举 + 真实 lrzsz 互通)
# ---------------------------------------------------------------------------
step "cargo test (host: ${HOST_TARGET})"
cargo test --workspace --target "${HOST_TARGET}"

# ---------------------------------------------------------------------------
# 3. 目标构建: 默认配置 (debug = dev_profile 全功能; release = 按配置裁剪)
# ---------------------------------------------------------------------------
step "cargo clippy (debug, 默认配置 = dev_profile 全功能)"
cargo clippy --workspace --target "${TARGET}" -- -D warnings

step "cargo build (debug)"
cargo build --target "${TARGET}"

step "cargo build (release, 默认裁剪配置)"
cargo build --release --target "${TARGET}"

# 尺寸测量紧接着各自构建之后 (构建产物路径相同, 会被后续构建覆盖)
SIZE_BIN="$(command -v arm-none-eabi-size || true)"
if [[ -z "${SIZE_BIN}" && -n "${ARM_TOOLCHAIN:-}" ]]; then
    SIZE_BIN="${ARM_TOOLCHAIN}/bin/arm-none-eabi-size"
fi
if [[ -n "${SIZE_BIN}" ]]; then
    step "体积报告 (默认 release 裁剪配置)"
    "${SIZE_BIN}" "target/${TARGET}/release/hc32f460"
fi

if [[ -n "${QUICK}" ]]; then
    step "quick 模式完成 (跳过全开配置)"
    exit 0
fi

# ---------------------------------------------------------------------------
# 4. 目标构建: 全开配置 (release, 所有大功能编译期开启)
#    经环境变量覆盖 .cargo/config.toml [env] 的默认值 (未设置 force 时
#    现有环境变量优先)。soak 依赖的 selftest 由 build.rs 自动随带。
# ---------------------------------------------------------------------------
step "cargo build (release, 全开配置: nano+selftest+soak+zmodem+can)"
CFG_SHELL_NANO_ENABLE=true \
CFG_SHELL_ZMODEM_ENABLE=true \
CFG_APP_SELFTEST_ENABLE=true \
CFG_SOAK_ENABLE=true \
CFG_CAN_ENABLE=true \
    cargo build --release --target "${TARGET}"

if [[ -n "${SIZE_BIN}" ]]; then
    step "体积报告 (全开 release 配置)"
    "${SIZE_BIN}" "target/${TARGET}/release/hc32f460"
fi

step "cargo clippy (全开配置, 与上一步同源) (CI 中另跑 --release 变体)"
CFG_SHELL_NANO_ENABLE=true \
CFG_SHELL_ZMODEM_ENABLE=true \
CFG_APP_SELFTEST_ENABLE=true \
CFG_SOAK_ENABLE=true \
CFG_CAN_ENABLE=true \
    cargo clippy --workspace --target "${TARGET}" -- -D warnings

step "cargo build (debug, 全开配置, 与 release 全开同源验证)"
CFG_SHELL_NANO_ENABLE=true \
CFG_SHELL_ZMODEM_ENABLE=true \
CFG_APP_SELFTEST_ENABLE=true \
CFG_SOAK_ENABLE=true \
CFG_CAN_ENABLE=true \
    cargo build --target "${TARGET}"

if [[ -z "${SIZE_BIN}" ]]; then
    echo "提示: 未找到 arm-none-eabi-size, 跳过体积报告 (设置 ARM_TOOLCHAIN 前缀或将其加入 PATH)"
fi

echo
echo "全部验证通过 ✔"
