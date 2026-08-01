MEMORY
{
    FLASH       (rwx) : ORIGIN = 0x00000000, LENGTH = 512K
    OTP_DATA    (rw)  : ORIGIN = 0x03000C00, LENGTH = 960
    OTP_LOCK    (rw)  : ORIGIN = 0x03000FC0, LENGTH = 60
    RAM         (rwx) : ORIGIN = 0x1FFF8000, LENGTH = 188K
    RET_RAM     (rwx) : ORIGIN = 0x200F0000, LENGTH = 4K
}

ENTRY(reset_handler)

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

    .text :
    {
        . = ALIGN(4);
        *(.text*);
        *(.rodata*);
        /* panic 处理器: 固定保留, 便于 gdb 定位 */
        KEEP(*(.text.panic_handler));
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

    .ARM.extab :
    {
        *(.ARM.extab*);
    } > FLASH

    .ARM.exidx :
    {
        *(.ARM.exidx*);
    } > FLASH
}