MEMORY
{
    FLASH       (rx)  : ORIGIN = 0x00000000, LENGTH = 512K
    OTP_DATA    (rw)  : ORIGIN = 0x03000C00, LENGTH = 960
    OTP_LOCK    (rw)  : ORIGIN = 0x03000FC0, LENGTH = 60
    RAM         (rwx) : ORIGIN = 0x1FFF8000, LENGTH = 188K
    RET_RAM     (rwx) : ORIGIN = 0x200F0000, LENGTH = 4K
}

ENTRY(reset_handler)

/* 栈预留大小: 栈顶 = RAM 顶 (0x20027000), 向下预留 STACK_SIZE 给栈,
 * 其余空间 (bss 之后) 全部划给堆。栈向堆溢出时会被堆数据覆盖,
 * 预留大小需按应用栈深度调整。 */
STACK_SIZE = 8K;

SECTIONS
{
    .vector_table :
    {
        /* 堆栈指针 */
        LONG(ORIGIN(RAM) + LENGTH(RAM));
        
        /* 复位向量 */
        KEEP(*(.vector_table.reset_vector));
        
        /* 系统异常向量 */
        KEEP(*(.vector_table.exceptions));

        /* 外设中断向量 */
        KEEP(*(.vector_table.interrupts));
    } > FLASH

    .icg 0x00000400 :
    {
        /* 硬件初始化配置 */
        KEEP(*(.icgs));
    } > FLASH

    .const :
    {
        . = ALIGN(4);
        *(.rodata*);
    }

    .text :
    {
        . = ALIGN(4);
        *(.text*);
        /* panic 处理器: 固定保留, 便于 gdb 定位 */
        KEEP(*(.text.panic_handler));
    } > FLASH

    .ARM.extab :
    {
        *(.ARM.extab*);
    } > FLASH

    .ARM.exidx :
    {
        *(.ARM.exidx*);
    } > FLASH

    _data_load_addr = LOADADDR(.data);

    .data :
    {
        . = ALIGN(4);
        _data_ram_start = .;
        *(.data*);
        _data_ram_end = .;
    } > RAM AT > FLASH

    .bss :
    {
        . = ALIGN(4);
        _bss_ram_start = .;
        *(.bss*);
        . = ALIGN(4);
        _bss_ram_end = .;
    } > RAM

    /* 堆: bss 之后, 栈预留区之前 */
    .heap (NOLOAD) :
    {
        . = ALIGN(8);
        _heap_start = .;
    } > RAM
}

/* 堆上界 = RAM 顶 - 栈预留 - 主栈 canary 4B (空闲线程巡检, 见 thread.rs) */
_heap_end = ORIGIN(RAM) + LENGTH(RAM) - STACK_SIZE - 4;
ASSERT(_heap_end >= _heap_start, "RAM 空间不足以容纳堆和栈预留");