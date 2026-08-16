//! 编译期工程配置: 全部集中定义于 `.cargo/config.toml` 的 `[env]` 段,
//! 经 `env!` 编译期读取。字符串→类型映射全部为 const 求值,
//! **非法值在编译期报错** (const 求值失败, 错误信息会指明具体常量),
//! 无需运行时校验。
//!
//! 使用方式: 各模块读 `crate::config::*` 类型化常量,
//! 修改 `.cargo/config.toml` 后重新编译即生效 (cargo 自动追踪该文件)。
//!
//! 注: UART/CAN/LED 的端口类型 (PortA/PortB/PortC) 由 Rust 类型系统编码, 固定在
//! 代码中; 引脚号/功能号等数值参数可在此配置。

use crate::can;
use crate::clk;
use crate::gpio;
use crate::uart;

// ============================== 编译期解析工具 ==============================
// const 上下文的 panic! 只能使用字面量消息 (不允许格式化参数)。

/// 编译期解析十进制整数字符串 (支持 `_` 分隔; 非法字符/溢出 → 编译报错)
const fn parse_u32(s: &str) -> u32 {
    let bytes = s.as_bytes();
    assert!(!bytes.is_empty(), "非法配置值: 整数不能为空");
    let mut i = 0;
    let mut v: u64 = 0;
    while i < bytes.len() {
        let b = bytes[i];
        assert!(
            b == b'_' || (b >= b'0' && b <= b'9'),
            "非法配置值: 应为十进制整数"
        );
        if b == b'_' {
            assert!(
                i > 0 && i + 1 < bytes.len() && bytes[i - 1] != b'_',
                "非法配置值: `_` 只能分隔数字"
            );
        } else {
            v = v * 10 + (b - b'0') as u64;
        }
        i += 1;
    }
    assert!(v <= u32::MAX as u64, "非法配置值: 溢出 u32");
    v as u32
}

/// 编译期解析十进制或 `0x` 前缀十六进制整数，支持 `_` 分隔。
const fn parse_u32_auto(s: &str) -> u32 {
    let bytes = s.as_bytes();
    assert!(!bytes.is_empty(), "非法配置值: 整数不能为空");
    let (mut i, radix) =
        if bytes.len() > 2 && bytes[0] == b'0' && (bytes[1] == b'x' || bytes[1] == b'X') {
            (2, 16u64)
        } else {
            (0, 10u64)
        };
    let digit_start = i;
    let mut value = 0u64;
    let mut digits = 0usize;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte == b'_' {
            assert!(
                i > digit_start && i + 1 < bytes.len() && bytes[i - 1] != b'_',
                "非法配置值: `_` 只能分隔数字"
            );
        } else {
            let digit = if byte >= b'0' && byte <= b'9' {
                (byte - b'0') as u64
            } else if radix == 16 && byte >= b'a' && byte <= b'f' {
                (byte - b'a' + 10) as u64
            } else if radix == 16 && byte >= b'A' && byte <= b'F' {
                (byte - b'A' + 10) as u64
            } else {
                panic!("非法配置值: 应为十进制或 0x 十六进制整数")
            };
            assert!(digit < radix, "非法配置值: 数字超出进制范围");
            value = value * radix + digit;
            assert!(value <= u32::MAX as u64, "非法配置值: 溢出 u32");
            digits += 1;
        }
        i += 1;
    }
    assert!(digits != 0, "非法配置值: 整数不能为空");
    value as u32
}

/// 是否为非空的可打印 ASCII 字符串。
const fn is_printable_ascii(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] < b' ' || bytes[i] > b'~' {
            return false;
        }
        i += 1;
    }
    true
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

const fn parse_bool(s: &str) -> bool {
    if eq_str(s, "true") {
        true
    } else if eq_str(s, "false") {
        false
    } else {
        panic!("非法配置值: 布尔值应为 true/false")
    }
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
const _: () = assert!(PLL_SRC <= 1, "CFG_PLL_SRC 非法 (可用 0/1)");
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

/// PLL 输入/VCO 范围校验。XTAL 与 HRC 频率都由编译期配置确定，使用
/// `u64` 保证错误的高倍频组合也不会在校验表达式中先发生整数回绕。
const fn assert_pll_range(source_hz: u32, m: u32, n: u32) {
    let source_hz = source_hz as u64;
    let input_div = m as u64 + 1;
    let vco_numerator = source_hz * (n as u64 + 1);
    assert!(
        source_hz >= 1_000_000 * input_div && source_hz <= 25_000_000 * input_div,
        "PLL 输入频率 source/(M+1) 应为 1~25MHz"
    );
    assert!(
        vco_numerator >= 240_000_000 * input_div && vco_numerator <= 480_000_000 * input_div,
        "PLL VCO 输出频率应为 240~480MHz"
    );
}

const _: () = assert_pll_width(PLL_M, PLL_N, PLL_P, PLL_Q, PLL_R);
const _: () = assert_pll_range(
    if PLL_SRC == 0 {
        XTAL_HZ
    } else {
        HRC_FREQ_MHZ * 1_000_000
    },
    PLL_M,
    PLL_N,
);
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
const _: () = assert!(TICKS_PER_SEC > 0, "CFG_TICKS_PER_SEC 必须大于 0");
/// 两者必须一致 (编译期校验)
const _: () = assert!(
    SYSTICK_FREQ_HZ == TICKS_PER_SEC,
    "CFG_SYSTICK_HZ 必须与 CFG_TICKS_PER_SEC 一致"
);
/// 优先级数量 (0 = 最高) (CFG_PRIORITY_MAX)
pub const PRIORITY_MAX: u8 = parse_u8(env!("CFG_PRIORITY_MAX"));
const _: () = assert!(
    PRIORITY_MAX >= 1 && PRIORITY_MAX <= 32,
    "CFG_PRIORITY_MAX 必须在 1~32 范围内"
);
/// 空闲线程优先级 (最低) (CFG_IDLE_PRIORITY)
pub const IDLE_PRIORITY: u8 = parse_u8(env!("CFG_IDLE_PRIORITY"));
const _: () = assert!(
    IDLE_PRIORITY as u16 + 1 == PRIORITY_MAX as u16,
    "CFG_IDLE_PRIORITY 必须等于 CFG_PRIORITY_MAX - 1"
);
/// 空闲线程栈大小 (字节) (CFG_IDLE_STACK)
pub const IDLE_STACK_SIZE: usize = parse_u32(env!("CFG_IDLE_STACK")) as usize;
const _: () = assert!(
    IDLE_STACK_SIZE >= 256,
    "CFG_IDLE_STACK 不得小于线程最小栈 256 字节"
);
const _: () = assert!(
    IDLE_STACK_SIZE.is_multiple_of(8),
    "CFG_IDLE_STACK 必须按 8 字节对齐"
);

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
const _: () = assert!(UART_BAUDRATE > 0, "CFG_UART_BAUDRATE 必须大于 0");
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
const _: () = assert!(
    UART_RX_BUF_SIZE >= 16 && UART_RX_BUF_SIZE <= 4096,
    "CFG_UART_RX_BUF_SIZE 应为 16~4096"
);
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

// ============================== [dma] ==============================

/// 是否启用 DMA 驱动与控制台 UART 发送卸载 (CFG_DMA_ENABLE)
///
/// 启用时 board 初始化路由 USART{`UART_UNIT`}_TI 事件到 TX 通道,
/// `Uart::write` 对达标长度的输出自动改用 DMA 整块发送。
pub const DMA_ENABLE: bool = if eq_str(env!("CFG_DMA_ENABLE"), "true") {
    true
} else if eq_str(env!("CFG_DMA_ENABLE"), "false") {
    false
} else {
    panic!("CFG_DMA_ENABLE 非法 (可用 true/false)")
};
/// 控制台 TX DMA 单元 (CFG_DMA_TX_UNIT = 1/2)
pub const DMA_TX_UNIT: u8 = parse_u8(env!("CFG_DMA_TX_UNIT"));
const _: () = assert!(
    DMA_TX_UNIT == 1 || DMA_TX_UNIT == 2,
    "CFG_DMA_TX_UNIT 非法 (可用 1/2)"
);
/// 控制台 TX DMA 通道 (CFG_DMA_TX_CHANNEL = 0~3)
pub const DMA_TX_CHANNEL: u8 = parse_u8(env!("CFG_DMA_TX_CHANNEL"));
const _: () = assert!(DMA_TX_CHANNEL <= 3, "CFG_DMA_TX_CHANNEL 非法 (可用 0~3)");
/// 触发 DMA 发送的最小输出长度 (字节) (CFG_DMA_TX_MIN)
///
/// 低于该阈值的输出 (逐字节轮询开销更小) 保持原轮询路径; 阈值同时
/// 保证 DMA 传输计数 (16 位) 的合理余量。
pub const DMA_TX_MIN: usize = parse_u32(env!("CFG_DMA_TX_MIN")) as usize;
const _: () = assert!(
    DMA_TX_MIN >= 4 && DMA_TX_MIN <= 512,
    "CFG_DMA_TX_MIN 应为 4~512"
);
/// 大块拷贝 (Flash→RAM / RAM→RAM) DMA 单元与通道 (CFG_DMA_COPY_UNIT /
/// CFG_DMA_COPY_CHANNEL)
pub const DMA_COPY_UNIT: u8 = parse_u8(env!("CFG_DMA_COPY_UNIT"));
const _: () = assert!(
    DMA_COPY_UNIT == 1 || DMA_COPY_UNIT == 2,
    "CFG_DMA_COPY_UNIT 非法 (可用 1/2)"
);
pub const DMA_COPY_CHANNEL: u8 = parse_u8(env!("CFG_DMA_COPY_CHANNEL"));
const _: () = assert!(
    DMA_COPY_CHANNEL <= 3,
    "CFG_DMA_COPY_CHANNEL 非法 (可用 0~3)"
);
/// TX 与 COPY 不得共用同一通道 (同一通道被两个功能同时配置会互相覆盖)
const _: () = assert!(
    !(DMA_ENABLE && DMA_TX_UNIT == DMA_COPY_UNIT && DMA_TX_CHANNEL == DMA_COPY_CHANNEL),
    "CFG_DMA_TX_* 与 CFG_DMA_COPY_* 不能配置到同一通道"
);
/// 触发 DMA 整块拷贝 (Flash→RAM / RAM→RAM) 的最小长度 (字节)
/// (CFG_DMA_COPY_MIN)
///
/// 低于该阈值的读取 (单次 DMA 配置开销反而更大) 保持逐字节/逐字回退。
pub const DMA_COPY_MIN: usize = parse_u32(env!("CFG_DMA_COPY_MIN")) as usize;
const _: () = assert!(
    DMA_COPY_MIN >= 16 && DMA_COPY_MIN <= 1024,
    "CFG_DMA_COPY_MIN 应为 16~1024"
);

// ============================== [can] ==============================

/// 是否在板级启动阶段初始化 CAN。默认关闭，因为本板仅引出 PB6/PB7，
/// 没有板载 CAN PHY；正常/外部回环模式需要外接收发器。
pub const CAN_ENABLE: bool = parse_bool(env!("CFG_CAN_ENABLE"));
/// 是否将 CAN 内部回环项目加入 `selftest`。
pub const CAN_SELFTEST_ENABLE: bool = parse_bool(env!("CFG_CAN_SELFTEST_ENABLE"));
pub const CAN_TX_PIN: u8 = parse_u8(env!("CFG_CAN_TX_PIN"));
pub const CAN_TX_FSEL: u8 = parse_u8(env!("CFG_CAN_TX_FSEL"));
pub const CAN_RX_PIN: u8 = parse_u8(env!("CFG_CAN_RX_PIN"));
pub const CAN_RX_FSEL: u8 = parse_u8(env!("CFG_CAN_RX_FSEL"));

const fn can_portb_supports_func_group2(pin: u8) -> bool {
    (pin >= 3 && pin <= 10) || (pin >= 12 && pin <= 15)
}

const _: () = assert!(
    can_portb_supports_func_group2(CAN_TX_PIN) && can_portb_supports_func_group2(CAN_RX_PIN),
    "CFG_CAN_TX_PIN/RX_PIN 必须是 PortB 上支持 Func_Grp2 的引脚"
);
const _: () = assert!(
    CAN_TX_PIN != CAN_RX_PIN,
    "CFG_CAN_TX_PIN 与 CFG_CAN_RX_PIN 不能相同"
);
const _: () = assert!(
    CAN_TX_FSEL == 50 && CAN_RX_FSEL == 51,
    "CFG_CAN_TX_FSEL/RX_FSEL 必须分别为 CAN Func50/Func51"
);

pub const CAN_BITRATE: u32 = parse_u32(env!("CFG_CAN_BITRATE"));
const _: () = assert!(
    CAN_BITRATE <= crate::can_timing::MAX_CLASSIC_BITRATE,
    "CFG_CAN_BITRATE 超过经典 CAN 1Mbit/s 上限"
);
pub const CAN_SAMPLE_POINT_PERMILLE: u16 = {
    let value = parse_u32(env!("CFG_CAN_SAMPLE_POINT_PERMILLE"));
    assert!(
        value <= u16::MAX as u32,
        "CFG_CAN_SAMPLE_POINT_PERMILLE 溢出 u16"
    );
    value as u16
};
pub const CAN_SJW: u8 = parse_u8(env!("CFG_CAN_SJW"));
pub const CAN_MAX_BITRATE_ERROR_PPM: u32 = parse_u32(env!("CFG_CAN_MAX_BITRATE_ERROR_PPM"));

const CAN_CONFIGURED_SYSTEM_CLOCK_HZ: u64 = match CLOCK_SOURCE {
    clk::ClockSource::Mrc => clk::MRC_HZ as u64,
    clk::ClockSource::Hrc => HRC_FREQ_MHZ as u64 * 1_000_000,
    clk::ClockSource::Xtal => XTAL_HZ as u64,
    clk::ClockSource::Pll => {
        let source_hz = if PLL_SRC == 0 {
            XTAL_HZ as u64
        } else {
            HRC_FREQ_MHZ as u64 * 1_000_000
        };
        source_hz * (PLL_N as u64 + 1) / (PLL_M as u64 + 1) / (PLL_P as u64 + 1)
    }
};
const CAN_CONFIGURED_EXCLK_HZ: u64 = CAN_CONFIGURED_SYSTEM_CLOCK_HZ / DIV_EXCLK as u64;
const _: () = assert!(
    !(CAN_ENABLE || CAN_SELFTEST_ENABLE) || CAN_CONFIGURED_EXCLK_HZ * 2 >= XTAL_HZ as u64 * 3,
    "配置的 EXCLK 必须不低于 1.5 倍 CANCLK(XTAL)"
);

pub const CAN_BIT_TIMING: crate::can_timing::BitTiming = match crate::can_timing::calculate(
    XTAL_HZ,
    CAN_BITRATE,
    CAN_SAMPLE_POINT_PERMILLE as u32,
    CAN_SJW as u32,
    CAN_MAX_BITRATE_ERROR_PPM,
) {
    Some(value) => value,
    None => panic!("CFG_CAN 位时序无法满足 DDL 约束或误差上限"),
};

pub const CAN_MODE: can::WorkMode = if eq_str(env!("CFG_CAN_MODE"), "normal") {
    can::WorkMode::Normal
} else if eq_str(env!("CFG_CAN_MODE"), "silent") {
    can::WorkMode::Silent
} else if eq_str(env!("CFG_CAN_MODE"), "internal-loopback") {
    can::WorkMode::InternalLoopback
} else if eq_str(env!("CFG_CAN_MODE"), "external-loopback") {
    can::WorkMode::ExternalLoopback
} else if eq_str(env!("CFG_CAN_MODE"), "external-loopback-silent") {
    can::WorkMode::ExternalLoopbackSilent
} else {
    panic!(
        "CFG_CAN_MODE 非法 (可用 normal/silent/internal-loopback/external-loopback/external-loopback-silent)"
    )
};
pub const CAN_PTB_SINGLE_SHOT: bool = parse_bool(env!("CFG_CAN_PTB_SINGLE_SHOT"));
pub const CAN_STB_SINGLE_SHOT: bool = parse_bool(env!("CFG_CAN_STB_SINGLE_SHOT"));
pub const CAN_STB_PRIORITY: can::StbPriority = if eq_str(env!("CFG_CAN_STB_PRIORITY"), "fifo") {
    can::StbPriority::Fifo
} else if eq_str(env!("CFG_CAN_STB_PRIORITY"), "id") {
    can::StbPriority::LowestIdFirst
} else {
    panic!("CFG_CAN_STB_PRIORITY 非法 (可用 fifo/id)")
};
pub const CAN_RX_WARN_LIMIT: u8 = parse_u8(env!("CFG_CAN_RX_WARN_LIMIT"));
pub const CAN_ERROR_WARN_LIMIT: u8 = parse_u8(env!("CFG_CAN_ERROR_WARN_LIMIT"));
const _: () = assert!(
    CAN_RX_WARN_LIMIT >= 1 && CAN_RX_WARN_LIMIT <= 10,
    "CFG_CAN_RX_WARN_LIMIT 应为 1~10"
);
const _: () = assert!(
    CAN_ERROR_WARN_LIMIT <= 15,
    "CFG_CAN_ERROR_WARN_LIMIT 应为 0~15"
);
pub const CAN_RX_ALL_FRAMES: bool = parse_bool(env!("CFG_CAN_RX_ALL_FRAMES"));
pub const CAN_RX_OVERFLOW: can::RxOverflowMode =
    if eq_str(env!("CFG_CAN_RX_OVERFLOW"), "overwrite-oldest") {
        can::RxOverflowMode::OverwriteOldest
    } else if eq_str(env!("CFG_CAN_RX_OVERFLOW"), "discard-newest") {
        can::RxOverflowMode::DiscardNewest
    } else {
        panic!("CFG_CAN_RX_OVERFLOW 非法 (可用 overwrite-oldest/discard-newest)")
    };
pub const CAN_SELF_ACK: bool = parse_bool(env!("CFG_CAN_SELF_ACK"));

pub const CAN_FILTER_TYPE: can::FilterType = if eq_str(env!("CFG_CAN_FILTER_TYPE"), "both") {
    can::FilterType::StandardAndExtended
} else if eq_str(env!("CFG_CAN_FILTER_TYPE"), "standard") {
    can::FilterType::StandardOnly
} else if eq_str(env!("CFG_CAN_FILTER_TYPE"), "extended") {
    can::FilterType::ExtendedOnly
} else {
    panic!("CFG_CAN_FILTER_TYPE 非法 (可用 both/standard/extended)")
};
pub const CAN_FILTER_ID: u32 = parse_u32_auto(env!("CFG_CAN_FILTER_ID"));
pub const CAN_FILTER_MASK: u32 = parse_u32_auto(env!("CFG_CAN_FILTER_MASK"));
const fn assert_can_filter(id: u32, mask: u32, kind: can::FilterType) {
    assert!(
        id <= 0x1FFF_FFFF && mask <= 0x1FFF_FFFF,
        "CFG_CAN_FILTER_ID/MASK 必须是 29 位 CAN ID"
    );
    assert!(
        !matches!(kind, can::FilterType::StandardOnly) || id <= 0x7FF,
        "standard 筛选器的 CFG_CAN_FILTER_ID 必须是 11 位"
    );
}
const _: () = assert_can_filter(CAN_FILTER_ID, CAN_FILTER_MASK, CAN_FILTER_TYPE);
pub static CAN_FILTERS: [can::Filter; 1] = [can::Filter {
    id: CAN_FILTER_ID,
    mask: CAN_FILTER_MASK,
    kind: CAN_FILTER_TYPE,
}];

/// selftest 轮询收发的真实 RTOS 超时，不是 CPU 忙等次数。
pub const CAN_TIMEOUT_MS: u32 = parse_u32(env!("CFG_CAN_TIMEOUT_MS"));
const _: () = assert!(
    CAN_TIMEOUT_MS >= 1 && CAN_TIMEOUT_MS <= 5000,
    "CFG_CAN_TIMEOUT_MS 应为 1~5000"
);

pub const CAN_CONFIG: can::Config = can::Config {
    mode: CAN_MODE,
    bitrate: CAN_BITRATE,
    sample_point_permille: CAN_SAMPLE_POINT_PERMILLE,
    sjw: CAN_SJW,
    max_bitrate_error_ppm: CAN_MAX_BITRATE_ERROR_PPM,
    filters: &CAN_FILTERS,
    ptb_single_shot: CAN_PTB_SINGLE_SHOT,
    stb_single_shot: CAN_STB_SINGLE_SHOT,
    stb_priority: CAN_STB_PRIORITY,
    rx_warn_limit: CAN_RX_WARN_LIMIT,
    error_warn_limit: CAN_ERROR_WARN_LIMIT,
    rx_all_frames: CAN_RX_ALL_FRAMES,
    rx_overflow: CAN_RX_OVERFLOW,
    self_ack: CAN_SELF_ACK,
    interrupts: can::Interrupts::ALL,
};

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

// ============================== [zmodem] ==============================

/// ZMODEM 帧/字节间等待超时 (毫秒) (CFG_ZMODEM_TIMEOUT_MS)
pub const ZMODEM_TIMEOUT_MS: u32 = parse_u32(env!("CFG_ZMODEM_TIMEOUT_MS"));
const _: () = assert!(
    ZMODEM_TIMEOUT_MS >= 100 && ZMODEM_TIMEOUT_MS <= 60000,
    "CFG_ZMODEM_TIMEOUT_MS 应为 100~60000"
);
/// ZMODEM 发送子包长度 (字节) (CFG_ZMODEM_SUBPACKET)
pub const ZMODEM_SUBPACKET: usize = parse_u32(env!("CFG_ZMODEM_SUBPACKET")) as usize;
const _: () = assert!(
    ZMODEM_SUBPACKET >= 32 && ZMODEM_SUBPACKET <= 1024,
    "CFG_ZMODEM_SUBPACKET 应为 32~1024"
);
/// 接收缓冲上限 = 单个接收文件大小上限 (字节) (CFG_ZMODEM_RX_MAX)
///
/// 快照文件系统整文件原子写入, 接收文件必须先完整缓存在 RAM;
/// 上限同时约束堆分配, 超过该大小的远端文件会被跳过。
pub const ZMODEM_RX_MAX: usize = parse_u32(env!("CFG_ZMODEM_RX_MAX")) as usize;
const _: () = assert!(
    ZMODEM_RX_MAX >= 1024 && ZMODEM_RX_MAX <= 65_472,
    "CFG_ZMODEM_RX_MAX 应为 1024~65472 (快照容量上限)"
);

// ============================== [shell] ==============================
/// 登录用户名 / 密码 (CFG_SHELL_USERNAME / CFG_SHELL_PASSWORD)
pub const SHELL_USERNAME: &str = env!("CFG_SHELL_USERNAME");
pub const SHELL_PASSWORD: &str = env!("CFG_SHELL_PASSWORD");
/// 登录失败允许次数 (CFG_SHELL_LOGIN_TRIES)
pub const SHELL_LOGIN_TRIES: u32 = parse_u32(env!("CFG_SHELL_LOGIN_TRIES"));
/// 输入行缓冲区大小 (字节) (CFG_SHELL_LINE_BUF)
pub const SHELL_LINE_BUF_SIZE: usize = parse_u32(env!("CFG_SHELL_LINE_BUF")) as usize;
/// RAM 中保留的历史命令条数 (CFG_SHELL_HISTORY_SIZE)
pub const SHELL_HISTORY_SIZE: usize = parse_u32(env!("CFG_SHELL_HISTORY_SIZE")) as usize;
const _: () = assert!(
    SHELL_LINE_BUF_SIZE >= 32 && SHELL_LINE_BUF_SIZE <= 256,
    "CFG_SHELL_LINE_BUF 应为 32~256"
);
const _: () = assert!(
    SHELL_LOGIN_TRIES >= 1 && SHELL_LOGIN_TRIES <= 100,
    "CFG_SHELL_LOGIN_TRIES 应为 1~100"
);
const _: () = assert!(
    is_printable_ascii(SHELL_USERNAME)
        && SHELL_USERNAME.len() <= SHELL_LINE_BUF_SIZE
        && SHELL_USERNAME.as_bytes()[0] != b' '
        && SHELL_USERNAME.as_bytes()[SHELL_USERNAME.len() - 1] != b' ',
    "CFG_SHELL_USERNAME 必须为非空可打印 ASCII，且不能首尾为空格或超过行缓冲"
);
const _: () = assert!(
    is_printable_ascii(SHELL_PASSWORD) && SHELL_PASSWORD.len() <= SHELL_LINE_BUF_SIZE,
    "CFG_SHELL_PASSWORD 必须为非空可打印 ASCII，且不能超过行缓冲"
);
const _: () = assert!(
    SHELL_HISTORY_SIZE >= 1 && SHELL_HISTORY_SIZE <= 16,
    "CFG_SHELL_HISTORY_SIZE 应为 1~16"
);
/// nano 风格编辑器无法自动探测 ANSI 终端时使用的回退宽度与高度。
pub const NANO_COLUMNS: usize = parse_u32(env!("CFG_NANO_COLUMNS")) as usize;
pub const NANO_ROWS: usize = parse_u32(env!("CFG_NANO_ROWS")) as usize;
/// 编辑器缓冲区上限。文件系统的实际剩余容量仍由写入预检决定。
pub const NANO_MAX_BYTES: usize = parse_u32(env!("CFG_NANO_MAX_BYTES")) as usize;
const _: () = assert!(
    NANO_COLUMNS >= 40 && NANO_COLUMNS <= 240,
    "CFG_NANO_COLUMNS 应为 40~240"
);
const _: () = assert!(
    NANO_ROWS >= 8 && NANO_ROWS <= 100,
    "CFG_NANO_ROWS 应为 8~100"
);
const _: () = assert!(
    NANO_MAX_BYTES >= 256 && NANO_MAX_BYTES <= 65_472,
    "CFG_NANO_MAX_BYTES 应为 256~65472 (快照容量上限)"
);

// ============================== [mpu] ==============================

/// 内存保护单元 (CFG_MPU_ENABLE = true/false)
///
/// 静态区域保护 (FLASH 只读 / SRAM+外设 XN) + 线程栈守卫
/// (硬件捕获栈溢出)。正确代码不受影响, 默认开启。
pub const MPU_ENABLE: bool = if eq_str(env!("CFG_MPU_ENABLE"), "true") {
    true
} else if eq_str(env!("CFG_MPU_ENABLE"), "false") {
    false
} else {
    panic!("CFG_MPU_ENABLE 非法 (可用 true/false)")
};
/// 栈守卫区大小 (字节) (CFG_MPU_STACK_GUARD)
///
/// 线程栈底与主栈 (ISR) 下方各一块无访问区域; 须与 `link.ld` 的
/// `MPU_GUARD_SIZE` 一致 (build.rs 编译期校验)。2 的幂, 32 字节起。
pub const MPU_STACK_GUARD: usize = parse_u32(env!("CFG_MPU_STACK_GUARD")) as usize;
const _: () = assert!(
    MPU_STACK_GUARD.is_power_of_two() && MPU_STACK_GUARD >= 32 && MPU_STACK_GUARD <= 1024,
    "CFG_MPU_STACK_GUARD 应为 2 的幂且 32~1024"
);

// ============================== [wdt] ==============================

/// 硬件看门狗 (CFG_WDT_ENABLE = true/false; supervisor 线程喂狗)
///
/// 默认关闭: 调试器断点暂停期间 WDT 超时会复位目标板; 产品部署
/// 时开启。supervisor 必须保持最高 RTOS 优先级，避免正常长输出饿死。
pub const WDT_ENABLE: bool = if eq_str(env!("CFG_WDT_ENABLE"), "true") {
    true
} else if eq_str(env!("CFG_WDT_ENABLE"), "false") {
    false
} else {
    panic!("CFG_WDT_ENABLE 非法 (可用 true/false)")
};
/// WDT supervisor 栈、优先级及喂狗周期。
pub const WDT_STACK_SIZE: usize = parse_u32(env!("CFG_WDT_STACK")) as usize;
pub const WDT_PRIORITY: u8 = parse_u8(env!("CFG_WDT_PRIORITY"));
pub const WDT_FEED_INTERVAL_MS: u32 = parse_u32(env!("CFG_WDT_FEED_MS"));
const _: () = assert!(
    WDT_STACK_SIZE >= 256 && WDT_STACK_SIZE.is_multiple_of(8),
    "CFG_WDT_STACK 必须不小于 256 且按 8 字节对齐"
);
const _: () = assert!(
    WDT_PRIORITY == 0 && WDT_PRIORITY < PRIORITY_MAX,
    "CFG_WDT_PRIORITY 必须为最高优先级 0"
);
const _: () = assert!(
    WDT_FEED_INTERVAL_MS > 0 && WDT_FEED_INTERVAL_MS <= 500,
    "CFG_WDT_FEED_MS 必须在 1~500ms 范围内"
);

// ============================== [console] ==============================

/// 控制台整行输出的行间间隙 (毫秒) (CFG_CONSOLE_LINE_GAP_MS)。
///
/// 115200 无流控下, 连续突发输出会超过 USB 转串口 (CH340) 与 PC 端
/// 读取能力的组合缓冲, 导致丢字节 (行尾截断/乱码)。每行输出后让出
/// 该时长, PC 端可在间隙内读取; 0 = 不节流。
pub const CONSOLE_LINE_GAP_MS: u32 = parse_u32(env!("CFG_CONSOLE_LINE_GAP_MS"));
const _: () = assert!(
    CONSOLE_LINE_GAP_MS <= 50,
    "CFG_CONSOLE_LINE_GAP_MS 应为 0~50"
);

// ============================== [panic] ==============================

/// panic/fault 后的行为策略 (CFG_PANIC_STRATEGY = halt/reset)
///
/// - `halt`: 屏蔽中断后 wfi 死循环 (调试期推荐, 便于 gdb 现场检查);
/// - `reset`: 软复位重启 (产品部署推荐, 尽快恢复服务)。
/// 见 [`crate::panic::PanicStrategy`]。
pub const PANIC_STRATEGY: crate::panic::PanicStrategy =
    if eq_str(env!("CFG_PANIC_STRATEGY"), "halt") {
        crate::panic::PanicStrategy::Halt
    } else if eq_str(env!("CFG_PANIC_STRATEGY"), "reset") {
        crate::panic::PanicStrategy::Reset
    } else {
        panic!("CFG_PANIC_STRATEGY 非法 (可用 halt/reset)")
    };

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
/// 控制台日志是否带 ANSI 颜色 (CFG_LOG_COLOR = true/false)。
/// 落盘文件恒为无颜色纯文本, 与本开关无关; 纯文本终端可关闭。
pub const LOG_COLOR: bool = if eq_str(env!("CFG_LOG_COLOR"), "true") {
    true
} else if eq_str(env!("CFG_LOG_COLOR"), "false") {
    false
} else {
    panic!("CFG_LOG_COLOR 非法 (可用 true/false)")
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

// ============================== [logfile] ==============================

/// 日志落盘开关 (CFG_LOG_FILE_ENABLE = true/false): 日志同时保存到
/// `/log/` 目录; 运行时可经 shell `log file on|off` 切换, 重启后恢复默认。
pub const LOG_FILE_ENABLE: bool = if eq_str(env!("CFG_LOG_FILE_ENABLE"), "true") {
    true
} else if eq_str(env!("CFG_LOG_FILE_ENABLE"), "false") {
    false
} else {
    panic!("CFG_LOG_FILE_ENABLE 非法 (可用 true/false)")
};
/// RAM 日志缓冲容量 (字节) (CFG_LOG_RING)。缓冲按条目保存待落盘日志,
/// 满时丢弃最旧条目; 掉电/复位会丢失缓冲内容。
pub const LOG_RING: usize = parse_u32(env!("CFG_LOG_RING")) as usize;
const _: () = assert!(
    LOG_RING >= 512 && LOG_RING <= 16384,
    "CFG_LOG_RING 应为 512~16384"
);
/// 单个日志文件大小上限 (字节) (CFG_LOG_FILE_MAX)
pub const LOG_FILE_MAX: usize = parse_u32(env!("CFG_LOG_FILE_MAX")) as usize;
const _: () = assert!(
    LOG_FILE_MAX >= 512 && LOG_FILE_MAX <= 8192,
    "CFG_LOG_FILE_MAX 应为 512~8192"
);
/// 保留的日志文件数 (CFG_LOG_FILE_SLOTS): 每个文件 = 一次启动的一段日志
/// (`boot_<序号>[_<段号>].log`), 超预算时删除最旧文件; 总日志容量 ≈
/// MAX × SLOTS。
pub const LOG_FILE_SLOTS: usize = parse_u32(env!("CFG_LOG_FILE_SLOTS")) as usize;
const _: () = assert!(
    LOG_FILE_SLOTS >= 1 && LOG_FILE_SLOTS <= 8,
    "CFG_LOG_FILE_SLOTS 应为 1~8"
);
/// 日志落盘线程刷新间隔 (毫秒) (CFG_LOG_FLUSH_MS): 周期性把 RAM 缓冲
/// 写入 `/log/`; 间隔越小延迟越低, 但 Flash 写放大与磨损越大。
pub const LOG_FLUSH_MS: u32 = parse_u32(env!("CFG_LOG_FLUSH_MS"));
const _: () = assert!(
    LOG_FLUSH_MS >= 100 && LOG_FLUSH_MS <= 60000,
    "CFG_LOG_FLUSH_MS 应为 100~60000"
);
/// 日志落盘线程参数 (CFG_APP_LOGFILE_*): 优先级低于 shell/LED,
/// 只做短促的 RAM→Flash 搬运, 不抢占业务。
pub const APP_LOGFILE_STACK: usize = parse_u32(env!("CFG_APP_LOGFILE_STACK")) as usize;
pub const APP_LOGFILE_PRIORITY: u8 = parse_u8(env!("CFG_APP_LOGFILE_PRIORITY"));
pub const APP_LOGFILE_TIMESLICE: u32 = parse_u32(env!("CFG_APP_LOGFILE_TIMESLICE"));
const _: () = assert!(
    APP_LOGFILE_STACK >= 256 && APP_LOGFILE_STACK.is_multiple_of(8),
    "CFG_APP_LOGFILE_STACK 必须不小于 256 且按 8 字节对齐"
);
const _: () = assert!(
    APP_LOGFILE_PRIORITY < IDLE_PRIORITY && APP_LOGFILE_PRIORITY > APP_LED_PRIORITY,
    "CFG_APP_LOGFILE_PRIORITY 必须低于所有业务线程 (高于 idle)"
);

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
pub const APP_SHELL_STACK: usize = parse_u32(env!("CFG_APP_SHELL_STACK")) as usize;
pub const APP_SHELL_PRIORITY: u8 = parse_u8(env!("CFG_APP_SHELL_PRIORITY"));
pub const APP_SHELL_TIMESLICE: u32 = parse_u32(env!("CFG_APP_SHELL_TIMESLICE"));
/// 周期定时器周期 (ms) (CFG_APP_TIMER_PERIOD_MS)
pub const APP_TIMER_PERIOD_MS: u32 = parse_u32(env!("CFG_APP_TIMER_PERIOD_MS"));

const _: () = assert!(
    APP_LED_STACK >= 256 && APP_LED_STACK.is_multiple_of(8),
    "CFG_APP_LED_STACK 必须不小于 256 且按 8 字节对齐"
);
const _: () = assert!(
    APP_SHELL_STACK >= 256 && APP_SHELL_STACK.is_multiple_of(8),
    "CFG_APP_SHELL_STACK 必须不小于 256 且按 8 字节对齐"
);
const _: () = assert!(
    APP_LED_PRIORITY < IDLE_PRIORITY && APP_SHELL_PRIORITY < IDLE_PRIORITY,
    "CFG_APP_*_PRIORITY 必须高于 idle 且位于有效优先级范围"
);
const _: () = assert!(
    !WDT_ENABLE || (WDT_PRIORITY < APP_LED_PRIORITY && WDT_PRIORITY < APP_SHELL_PRIORITY),
    "启用 WDT 时 CFG_WDT_PRIORITY 必须高于所有应用线程"
);
const _: () = assert!(APP_LED_BLINK_MS > 0, "CFG_APP_LED_BLINK_MS 必须大于 0");
const _: () = assert!(
    APP_TIMER_PERIOD_MS > 0,
    "CFG_APP_TIMER_PERIOD_MS 必须大于 0"
);

// ============================== [soak] ==============================

/// soak 无参数时的默认时长 (分钟; 0 = 直到 ESC)
pub const SOAK_MINUTES: u32 = parse_u32(env!("CFG_SOAK_MINUTES"));
const _: () = assert!(
    SOAK_MINUTES <= 24 * 60 * 7,
    "CFG_SOAK_MINUTES 不应超过 7 天 (10080 分钟)"
);
/// 进度报告间隔 (毫秒)
pub const SOAK_REPORT_INTERVAL_MS: u32 = parse_u32(env!("CFG_SOAK_REPORT_INTERVAL_MS"));
const _: () = assert!(
    SOAK_REPORT_INTERVAL_MS >= 1000,
    "CFG_SOAK_REPORT_INTERVAL_MS 应不小于 1000"
);
/// 压力线程心跳停滞判定: 超过该时长无进展即判挂起 (毫秒)
pub const SOAK_HANG_GRACE_MS: u32 = parse_u32(env!("CFG_SOAK_HANG_GRACE_MS"));
const _: () = assert!(
    SOAK_HANG_GRACE_MS >= 1000,
    "CFG_SOAK_HANG_GRACE_MS 应不小于 1000"
);
/// Flash 压力节流: 两次擦写循环的最小间隔 (毫秒, 保护 Flash 寿命)
pub const SOAK_FLASH_INTERVAL_MS: u32 = parse_u32(env!("CFG_SOAK_FLASH_INTERVAL_MS"));
const _: () = assert!(
    SOAK_FLASH_INTERVAL_MS >= 1000,
    "CFG_SOAK_FLASH_INTERVAL_MS 应不小于 1000"
);

/// 测试报告预算: `/test/` 目录保留的报告文件数 (超出删除最旧;
/// 文件名按字典序即时间序)
pub const TEST_REPORT_SLOTS: u32 = parse_u32(env!("CFG_TEST_REPORT_SLOTS"));
const _: () = assert!(
    TEST_REPORT_SLOTS >= 1 && TEST_REPORT_SLOTS <= 32,
    "CFG_TEST_REPORT_SLOTS 应为 1~32"
);
