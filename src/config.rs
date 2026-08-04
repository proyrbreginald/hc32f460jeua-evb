//! 编译期工程配置: 全部集中定义于 `.cargo/config.toml` 的 `[env]` 段,
//! 经 `env!` 编译期读取。字符串→类型映射全部为 const 求值,
//! **非法值在编译期报错** (const 求值失败, 错误信息会指明具体常量),
//! 无需运行时校验。
//!
//! 使用方式: 各模块读 `crate::config::*` 类型化常量,
//! 修改 `.cargo/config.toml` 后重新编译即生效 (cargo 自动追踪该文件)。
//!
//! 注: UART/LED 的端口类型 (PortA/PortC) 由 Rust 类型系统编码, 固定在
//! 代码中; 引脚号/功能号等数值参数可在此配置。

use crate::clk;
use crate::gpio;
use crate::uart;

// ============================== 编译期解析工具 ==============================
// const 上下文的 panic! 只能使用字面量消息 (不允许格式化参数)。

/// 编译期解析十进制整数字符串 (支持 `_` 分隔; 非法字符/溢出 → 编译报错)
const fn parse_u32(s: &str) -> u32 {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut v: u64 = 0;
    while i < bytes.len() {
        let b = bytes[i];
        assert!(
            b == b'_' || (b >= b'0' && b <= b'9'),
            "非法配置值: 应为十进制整数"
        );
        if b != b'_' {
            v = v * 10 + (b - b'0') as u64;
        }
        i += 1;
    }
    assert!(v <= u32::MAX as u64, "非法配置值: 溢出 u32");
    v as u32
}

/// 编译期解析 u8 (超出 0~255 → 编译报错)
const fn parse_u8(s: &str) -> u8 {
    let v = parse_u32(s);
    assert!(v <= 255, "非法配置值: 超出 0~255");
    v as u8
}

/// 编译期字符串比较 (str 的 PartialEq 尚未 const 稳定, 用字节逐位比较)
const fn eq_str(a: &str, b: &str) -> bool {
    let (x, y) = (a.as_bytes(), b.as_bytes());
    if x.len() != y.len() {
        return false;
    }
    let mut i = 0;
    while i < x.len() {
        if x[i] != y[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// 编译期校验总线分频系数 (1/2/4/8/16)
const fn assert_div(v: u32) {
    match v {
        1 | 2 | 4 | 8 | 16 => {}
        _ => panic!("总线分频非法 (可用 1/2/4/8/16)"),
    }
}

// ============================== [board] ==============================

/// 芯片型号 (CFG_CHIP_MODEL)
pub const CHIP_MODEL: &str = env!("CFG_CHIP_MODEL");
/// 内核名 (CFG_CORE)
pub const CORE: &str = env!("CFG_CORE");

// ============================== [clk] ==============================

/// 外部高速晶振频率 (Hz), 合法范围 4~25MHz (CFG_XTAL_HZ)
pub const XTAL_HZ: u32 = parse_u32(env!("CFG_XTAL_HZ"));
const _: () = assert!(
    XTAL_HZ >= 4_000_000 && XTAL_HZ <= 25_000_000,
    "CFG_XTAL_HZ 超出合法范围 4~25MHz"
);
/// 系统时钟源 (CFG_CLK_SOURCE = mrc/hrc/xtal/pll)
pub const CLOCK_SOURCE: clk::ClockSource = if eq_str(env!("CFG_CLK_SOURCE"), "pll") {
    clk::ClockSource::Pll
} else if eq_str(env!("CFG_CLK_SOURCE"), "xtal") {
    clk::ClockSource::Xtal
} else if eq_str(env!("CFG_CLK_SOURCE"), "hrc") {
    clk::ClockSource::Hrc
} else if eq_str(env!("CFG_CLK_SOURCE"), "mrc") {
    clk::ClockSource::Mrc
} else {
    panic!("CFG_CLK_SOURCE 非法 (可用 mrc/hrc/xtal/pll)")
};
/// XTAL 稳定时间编码 (CFG_XTAL_STABLE_TIME = 1~9, ≈133µs~32ms,
/// 对齐 DDL CLK_XTAL_STB_*; 默认 5 = 2ms)
pub const XTAL_STABLE_TIME: u32 = parse_u32(env!("CFG_XTAL_STABLE_TIME"));
const _: () = assert!(
    XTAL_STABLE_TIME >= 1 && XTAL_STABLE_TIME <= 9,
    "CFG_XTAL_STABLE_TIME 非法 (可用 1~9)"
);
/// XTAL 驱动能力 (CFG_XTAL_DRV = ulow/low/mid/high, 编码对齐 DDL
/// CLK_XTAL_DRV_*: ulow 典型 4~8MHz 晶振)
pub const XTAL_DRV: u32 = if eq_str(env!("CFG_XTAL_DRV"), "ulow") {
    3
} else if eq_str(env!("CFG_XTAL_DRV"), "low") {
    2
} else if eq_str(env!("CFG_XTAL_DRV"), "mid") {
    1
} else if eq_str(env!("CFG_XTAL_DRV"), "high") {
    0
} else {
    panic!("CFG_XTAL_DRV 非法 (可用 ulow/low/mid/high)")
};
/// XTAL 超强驱动 (CFG_XTAL_SUPDRV = true/false, XTALCFGR.SUPDRV;
/// 对齐 DDL CLK_XTAL_SUPDRV_ON, 评估板默认开启)
pub const XTAL_SUPDRV: bool = if eq_str(env!("CFG_XTAL_SUPDRV"), "true") {
    true
} else if eq_str(env!("CFG_XTAL_SUPDRV"), "false") {
    false
} else {
    panic!("CFG_XTAL_SUPDRV 非法 (可用 true/false)")
};
/// HRC 频率 (MHz) (CFG_HRC_FREQ = 16/20)
///
/// 写入 flash 0x404 的 ICG1 配置字 (bit0 = HRCFREQSEL, 0=20MHz/1=16MHz),
/// 复位时由硬件载入运行期只读的 ICG1 寄存器 (见 `icg` 模块)。
pub const HRC_FREQ_MHZ: u32 = parse_u32(env!("CFG_HRC_FREQ"));
const _: () = assert!(
    HRC_FREQ_MHZ == 16 || HRC_FREQ_MHZ == 20,
    "CFG_HRC_FREQ 非法 (可用 16/20)"
);
/// HRC 复位后是否停止 (CFG_HRC_STOP = true/false; ICG1.HRCSTOP, 与
/// HRCFREQSEL 同写于 flash ICG1 配置字)
///
/// true = 复位后 HRC 停止 (默认安全, 需要时经 `clk::hrc_cmd` 启动);
/// false = 复位后持续振荡 (复位即运行, 启动零等待)。
pub const HRC_STOP: bool = if eq_str(env!("CFG_HRC_STOP"), "true") {
    true
} else if eq_str(env!("CFG_HRC_STOP"), "false") {
    false
} else {
    panic!("CFG_HRC_STOP 非法 (可用 true/false)")
};
/// MPLL 倍频分频 (CFG_PLL_*; PLLCLK = src ÷(m+1) ×(n+1) ÷(p+1))
pub const PLL_SRC: u32 = parse_u32(env!("CFG_PLL_SRC"));
pub const PLL_M: u32 = parse_u32(env!("CFG_PLL_M"));
pub const PLL_N: u32 = parse_u32(env!("CFG_PLL_N"));
pub const PLL_P: u32 = parse_u32(env!("CFG_PLL_P"));
pub const PLL_Q: u32 = parse_u32(env!("CFG_PLL_Q"));
pub const PLL_R: u32 = parse_u32(env!("CFG_PLL_R"));
// ---- MPLL 参数编译期校验 (对齐 DDL: 位宽 + VCO 输入/输出范围) ----
/// 位宽与有效倍频/分频范围 (寄存器值+1 才是实际值: N→20~480, P/Q/R→2~16)
const fn assert_pll_width(m: u32, n: u32, p: u32, q: u32, r: u32) {
    assert!(m <= 31, "CFG_PLL_M 超出寄存器位宽 (0~31)");
    assert!(n <= 511, "CFG_PLL_N 超出寄存器位宽 (0~511)");
    assert!(
        p <= 15 && q <= 15 && r <= 15,
        "CFG_PLL_P/Q/R 超出寄存器位宽 (0~15)"
    );
    assert!(n >= 19 && n <= 479, "CFG_PLL_N 对应倍频 (N+1) 应为 20~480");
    assert!(
        p >= 1 && p <= 15 && q >= 1 && q <= 15 && r >= 1 && r <= 15,
        "CFG_PLL_P/Q/R 对应分频 (值+1) 应为 2~16"
    );
}

/// XTAL 源 (PLL_SRC=0) 时的 VCO 输入/输出范围 (参考手册;
/// HRC 源频率运行时决定, 无法静态校验)
const fn assert_pll_xtal_range(xtal_hz: u32, m: u32, n: u32) {
    assert!(
        xtal_hz / (m + 1) >= 1_000_000 && xtal_hz / (m + 1) <= 25_000_000,
        "PLL 输入频率 XTAL/(M+1) 应为 1~25MHz"
    );
    assert!(
        xtal_hz / (m + 1) * (n + 1) >= 240_000_000 && xtal_hz / (m + 1) * (n + 1) <= 480_000_000,
        "PLL VCO 输出频率应为 240~480MHz"
    );
}

const _: () = assert_pll_width(PLL_M, PLL_N, PLL_P, PLL_Q, PLL_R);
const _: () = if PLL_SRC == 0 {
    assert_pll_xtal_range(XTAL_HZ, PLL_M, PLL_N)
};
/// 总线分频系数 (CFG_DIV_*), 非法值编译期报错
pub const DIV_HCLK: u32 = parse_u32(env!("CFG_DIV_HCLK"));
pub const DIV_PCLK0: u32 = parse_u32(env!("CFG_DIV_PCLK0"));
pub const DIV_PCLK1: u32 = parse_u32(env!("CFG_DIV_PCLK1"));
pub const DIV_PCLK2: u32 = parse_u32(env!("CFG_DIV_PCLK2"));
pub const DIV_PCLK3: u32 = parse_u32(env!("CFG_DIV_PCLK3"));
pub const DIV_PCLK4: u32 = parse_u32(env!("CFG_DIV_PCLK4"));
pub const DIV_EXCLK: u32 = parse_u32(env!("CFG_DIV_EXCLK"));
const _: () = assert_div(DIV_HCLK);
const _: () = assert_div(DIV_PCLK0);
const _: () = assert_div(DIV_PCLK1);
const _: () = assert_div(DIV_PCLK2);
const _: () = assert_div(DIV_PCLK3);
const _: () = assert_div(DIV_PCLK4);
const _: () = assert_div(DIV_EXCLK);

// ============================== [systick] / [rtos] ==============================

/// SysTick 中断频率 (Hz), RTOS 节拍源 (CFG_SYSTICK_HZ)
pub const SYSTICK_FREQ_HZ: u32 = parse_u32(env!("CFG_SYSTICK_HZ"));
/// RTOS 节拍频率 (Hz) (CFG_TICKS_PER_SEC)
pub const TICKS_PER_SEC: u32 = parse_u32(env!("CFG_TICKS_PER_SEC"));
/// 两者必须一致 (编译期校验)
const _: () = assert!(
    SYSTICK_FREQ_HZ == TICKS_PER_SEC,
    "CFG_SYSTICK_HZ 必须与 CFG_TICKS_PER_SEC 一致"
);
/// 优先级数量 (0 = 最高) (CFG_PRIORITY_MAX)
pub const PRIORITY_MAX: u8 = parse_u8(env!("CFG_PRIORITY_MAX"));
/// 空闲线程优先级 (最低) (CFG_IDLE_PRIORITY)
pub const IDLE_PRIORITY: u8 = parse_u8(env!("CFG_IDLE_PRIORITY"));
/// 空闲线程栈大小 (字节) (CFG_IDLE_STACK)
pub const IDLE_STACK_SIZE: usize = parse_u32(env!("CFG_IDLE_STACK")) as usize;

// ============================== [uart] ==============================

/// 控制台 USART 单元 (1~4, CFG_UART_UNIT), 编码在 const 泛型中
pub const UART_UNIT: u8 = parse_u8(env!("CFG_UART_UNIT"));
const _: () = assert!(
    UART_UNIT >= 1 && UART_UNIT <= 4,
    "CFG_UART_UNIT 非法 (可用 1~4)"
);
/// 控制台 UART 类型 (单元号编译期确定)
pub type ConsoleUart = uart::Uart<{ UART_UNIT }>;
/// TX 引脚号 / 功能号 (CFG_UART_TX_PIN / CFG_UART_TX_FSEL)
pub const UART_TX_PIN: u8 = parse_u8(env!("CFG_UART_TX_PIN"));
pub const UART_TX_FSEL: u8 = parse_u8(env!("CFG_UART_TX_FSEL"));
/// RX 引脚号 / 功能号 (CFG_UART_RX_PIN / CFG_UART_RX_FSEL)
pub const UART_RX_PIN: u8 = parse_u8(env!("CFG_UART_RX_PIN"));
pub const UART_RX_FSEL: u8 = parse_u8(env!("CFG_UART_RX_FSEL"));
/// 波特率 (bps) (CFG_UART_BAUDRATE)
pub const UART_BAUDRATE: u32 = parse_u32(env!("CFG_UART_BAUDRATE"));
/// 过采样 (CFG_UART_OVERSAMPLE = 8/16)
pub const UART_OVERSAMPLE: uart::Oversample = match parse_u32(env!("CFG_UART_OVERSAMPLE")) {
    8 => uart::Oversample::Eight,
    16 => uart::Oversample::Sixteen,
    _ => panic!("CFG_UART_OVERSAMPLE 非法 (可用 8/16)"),
};
/// 时钟预分频 (CFG_UART_CLOCK_DIV = 1/4/16/64)
pub const UART_CLOCK_DIV: uart::ClockDiv = match parse_u32(env!("CFG_UART_CLOCK_DIV")) {
    1 => uart::ClockDiv::Div1,
    4 => uart::ClockDiv::Div4,
    16 => uart::ClockDiv::Div16,
    64 => uart::ClockDiv::Div64,
    _ => panic!("CFG_UART_CLOCK_DIV 非法 (可用 1/4/16/64)"),
};
/// 数据位 (CFG_UART_DATA_BITS = 8/9)
pub const UART_DATA_BITS: uart::DataBits = match parse_u32(env!("CFG_UART_DATA_BITS")) {
    8 => uart::DataBits::Eight,
    9 => uart::DataBits::Nine,
    _ => panic!("CFG_UART_DATA_BITS 非法 (可用 8/9)"),
};
/// 校验位 (CFG_UART_PARITY = none/even/odd)
pub const UART_PARITY: uart::Parity = if eq_str(env!("CFG_UART_PARITY"), "none") {
    uart::Parity::None
} else if eq_str(env!("CFG_UART_PARITY"), "even") {
    uart::Parity::Even
} else if eq_str(env!("CFG_UART_PARITY"), "odd") {
    uart::Parity::Odd
} else {
    panic!("CFG_UART_PARITY 非法 (可用 none/even/odd)")
};
/// 停止位 (CFG_UART_STOP_BITS = 1/2)
pub const UART_STOP_BITS: uart::StopBits = match parse_u32(env!("CFG_UART_STOP_BITS")) {
    1 => uart::StopBits::One,
    2 => uart::StopBits::Two,
    _ => panic!("CFG_UART_STOP_BITS 非法 (可用 1/2)"),
};
/// 发送顺序 (CFG_UART_FIRST_BIT = lsb/msb)
pub const UART_FIRST_BIT: uart::FirstBit = if eq_str(env!("CFG_UART_FIRST_BIT"), "lsb") {
    uart::FirstBit::Lsb
} else if eq_str(env!("CFG_UART_FIRST_BIT"), "msb") {
    uart::FirstBit::Msb
} else {
    panic!("CFG_UART_FIRST_BIT 非法 (可用 lsb/msb)")
};
/// 起始位检测极性 (CFG_UART_START_POLARITY = falling/low; DDL 默认下降沿)
pub const UART_START_POLARITY: uart::StartBitPolarity =
    if eq_str(env!("CFG_UART_START_POLARITY"), "falling") {
        uart::StartBitPolarity::Falling
    } else if eq_str(env!("CFG_UART_START_POLARITY"), "low") {
        uart::StartBitPolarity::Low
    } else {
        panic!("CFG_UART_START_POLARITY 非法 (可用 falling/low)")
    };
/// 硬件流控 (CFG_UART_FLOW_CTRL = none/cts; RTS 为 F460 默认行为)
pub const UART_FLOW_CTRL: uart::FlowControl = if eq_str(env!("CFG_UART_FLOW_CTRL"), "none") {
    uart::FlowControl::None
} else if eq_str(env!("CFG_UART_FLOW_CTRL"), "cts") {
    uart::FlowControl::Cts
} else {
    panic!("CFG_UART_FLOW_CTRL 非法 (可用 none/cts)")
};
/// 噪声滤波 (CFG_UART_NOISE_FILTER = true/false, CR1.NFE)
pub const UART_NOISE_FILTER: bool = if eq_str(env!("CFG_UART_NOISE_FILTER"), "true") {
    true
} else if eq_str(env!("CFG_UART_NOISE_FILTER"), "false") {
    false
} else {
    panic!("CFG_UART_NOISE_FILTER 非法 (可用 true/false)")
};
/// 接收环形缓冲大小 (字节) (CFG_UART_RX_BUF_SIZE)
pub const UART_RX_BUF_SIZE: usize = parse_u32(env!("CFG_UART_RX_BUF_SIZE")) as usize;
/// INTC 中断通道 (CFG_UART_IRQ_CHANNEL, INT000~INT127; INT128+ 为共享线)
pub const UART_RX_IRQ_CHANNEL: usize = parse_u32(env!("CFG_UART_IRQ_CHANNEL")) as usize;
const _: () = assert!(
    UART_RX_IRQ_CHANNEL < 128,
    "CFG_UART_IRQ_CHANNEL 应为 INT000~INT127 (共享线 INT128+ 不支持)"
);
/// NVIC 抢占优先级 (CFG_UART_IRQ_PRIORITY, 0~15, 越小越高)
pub const UART_RX_IRQ_PRIORITY: u8 = parse_u8(env!("CFG_UART_IRQ_PRIORITY"));
const _: () = assert!(
    UART_RX_IRQ_PRIORITY <= 15,
    "CFG_UART_IRQ_PRIORITY 非法 (可用 0~15)"
);

// ============================== [gpio] ==============================

/// 板载 LED 引脚号 (CFG_LED_PIN; 端口 PortC 固定在代码中)
pub const LED_PIN: u8 = parse_u8(env!("CFG_LED_PIN"));
/// LED 初始电平 (CFG_LED_LEVEL = high/low)
pub const LED_INITIAL_LEVEL: gpio::Level = if eq_str(env!("CFG_LED_LEVEL"), "high") {
    gpio::Level::High
} else if eq_str(env!("CFG_LED_LEVEL"), "low") {
    gpio::Level::Low
} else {
    panic!("CFG_LED_LEVEL 非法 (可用 high/low)")
};

// ============================== [shell] ==============================

/// 登录用户名 / 密码 (CFG_SHELL_USERNAME / CFG_SHELL_PASSWORD)
pub const SHELL_USERNAME: &str = env!("CFG_SHELL_USERNAME");
pub const SHELL_PASSWORD: &str = env!("CFG_SHELL_PASSWORD");
/// 登录失败允许次数 (CFG_SHELL_LOGIN_TRIES)
pub const SHELL_LOGIN_TRIES: u32 = parse_u32(env!("CFG_SHELL_LOGIN_TRIES"));
/// 输入行缓冲区大小 (字节) (CFG_SHELL_LINE_BUF)
pub const SHELL_LINE_BUF_SIZE: usize = parse_u32(env!("CFG_SHELL_LINE_BUF")) as usize;

// ============================== [rtc] ==============================

/// 是否启用 RTC 并作为日志时间戳 (CFG_RTC_ENABLE = true/false)
///
/// 启用时 main 初始化 RTC (LRC 源, 24H, 基准 2000-01-01 00:00:00),
/// 日志输出带 `[天:时:分:秒]` 前缀 (自启动起的运行时长)。
pub const RTC_ENABLE: bool = if eq_str(env!("CFG_RTC_ENABLE"), "true") {
    true
} else if eq_str(env!("CFG_RTC_ENABLE"), "false") {
    false
} else {
    panic!("CFG_RTC_ENABLE 非法 (可用 true/false)")
};

// ============================== [log] ==============================

/// 日志默认开关 (CFG_LOG_ENABLE = true/false)
/// 运行时可经 shell `log on|off` 切换, 重启后恢复此默认值
pub const LOG_ENABLE: bool = if eq_str(env!("CFG_LOG_ENABLE"), "true") {
    true
} else if eq_str(env!("CFG_LOG_ENABLE"), "false") {
    false
} else {
    panic!("CFG_LOG_ENABLE 非法 (可用 true/false)")
};
/// 日志默认级别阈值 (CFG_LOG_LEVEL = error/warn/info/debug/trace)
/// 输出 ≤ 阈值的级别; 运行时可经 shell `log level <级别>` 调整
pub const LOG_LEVEL: crate::log::Level = if eq_str(env!("CFG_LOG_LEVEL"), "error") {
    crate::log::Level::Error
} else if eq_str(env!("CFG_LOG_LEVEL"), "warn") {
    crate::log::Level::Warn
} else if eq_str(env!("CFG_LOG_LEVEL"), "info") {
    crate::log::Level::Info
} else if eq_str(env!("CFG_LOG_LEVEL"), "debug") {
    crate::log::Level::Debug
} else if eq_str(env!("CFG_LOG_LEVEL"), "trace") {
    crate::log::Level::Trace
} else {
    panic!("CFG_LOG_LEVEL 非法 (可用 error/warn/info/debug/trace)")
};

/// 命令是否在启用列表中 (CFG_SHELL_COMMANDS, 逗号分隔, 忽略首尾空格)
///
/// 每个 shell 命令可单独通过该列表启用/禁用: 新增命令需在
/// `src/shell.rs` 命令表注册并加入此列表。const 求值, 结果编译期确定。
pub const fn cmd_enabled(name: &str) -> bool {
    let list = env!("CFG_SHELL_COMMANDS").as_bytes();
    let name = name.as_bytes();
    let mut i = 0;
    let mut start = 0;
    while i <= list.len() {
        if i == list.len() || list[i] == b',' {
            // 定位一项并去除首尾空格 (就地比较, 不做切片)
            let mut a = start;
            let mut b = i;
            while a < b && list[a] == b' ' {
                a += 1;
            }
            while b > a && list[b - 1] == b' ' {
                b -= 1;
            }
            if b - a == name.len() {
                let mut same = true;
                let mut j = 0;
                while j < name.len() {
                    if list[a + j] != name[j] {
                        same = false;
                    }
                    j += 1;
                }
                if same {
                    return true;
                }
            }
            start = i + 1;
        }
        i += 1;
    }
    false
}

// ============================== [app] ==============================

/// 演示线程参数 (CFG_APP_*)
pub const APP_LED_STACK: usize = parse_u32(env!("CFG_APP_LED_STACK")) as usize;
pub const APP_LED_PRIORITY: u8 = parse_u8(env!("CFG_APP_LED_PRIORITY"));
pub const APP_LED_TIMESLICE: u32 = parse_u32(env!("CFG_APP_LED_TIMESLICE"));
pub const APP_LED_BLINK_MS: u32 = parse_u32(env!("CFG_APP_LED_BLINK_MS"));
/// 是否启用内核自检 (CFG_APP_SELFTEST_ENABLE = true/false)
/// 启用后通过 shell 命令 `selftest` 手动启动, 不再开机自动运行
pub const APP_SELFTEST_ENABLE: bool = if eq_str(env!("CFG_APP_SELFTEST_ENABLE"), "true") {
    true
} else if eq_str(env!("CFG_APP_SELFTEST_ENABLE"), "false") {
    false
} else {
    panic!("CFG_APP_SELFTEST_ENABLE 非法 (可用 true/false)")
};
pub const APP_SHELL_STACK: usize = parse_u32(env!("CFG_APP_SHELL_STACK")) as usize;
pub const APP_SHELL_PRIORITY: u8 = parse_u8(env!("CFG_APP_SHELL_PRIORITY"));
pub const APP_SHELL_TIMESLICE: u32 = parse_u32(env!("CFG_APP_SHELL_TIMESLICE"));
/// 周期定时器周期 (ms) (CFG_APP_TIMER_PERIOD_MS)
pub const APP_TIMER_PERIOD_MS: u32 = parse_u32(env!("CFG_APP_TIMER_PERIOD_MS"));
