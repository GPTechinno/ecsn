/* With the `scripts` feature: what Bitcoin Core's C++ needs that cortex-m-rt's link.x leaves out.
   Placed before link.x discards them, these statements take the sections first. */
SECTIONS
{
  /* the exception tables, which C++ unwinds with (the script interpreter catches its own
     exceptions) */
  .ARM.extab : ALIGN(4)
  {
    *(.ARM.extab .ARM.extab.* .gnu.linkonce.armextab.*)
  } > FLASH

  .ARM.exidx : ALIGN(4)
  {
    __exidx_start = .;
    KEEP(*(.ARM.exidx .ARM.exidx.* .gnu.linkonce.armexidx.*))
    __exidx_end = .;
  } > FLASH

  /* the static constructors, which `cxx::run_constructors` calls at the start */
  .init_array : ALIGN(4)
  {
    __init_array_start = .;
    KEEP(*(SORT(.init_array.*)))
    KEEP(*(.init_array))
    __init_array_end = .;
  } > FLASH
} INSERT AFTER .rodata;
