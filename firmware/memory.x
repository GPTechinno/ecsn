/* STM32H747XI, Cortex-M7 core. Flash bank 2 (0x08100000) holds the Cortex-M4's program. */
MEMORY
{
    FLASH  : ORIGIN = 0x08000000, LENGTH = 1024K /* bank 1 */
    RAM    : ORIGIN = 0x24000000, LENGTH = 512K  /* AXI SRAM */
    RAM_D3 : ORIGIN = 0x38000000, LENGTH = 64K   /* SRAM4 */
}

SECTIONS
{
    .ram_d3 (NOLOAD) :
    {
        *(.ram_d3.shared_data)
        *(.ram_d3)
    } > RAM_D3
}
