OUTPUT_FORMAT(
    "elf64-loongarch",
    "elf64-loongarch",
    "elf64-loongarch"
)

OUTPUT_ARCH(loongarch)
ENTRY(_start)


SECTIONS
{
	KERNEL_VMA = 0x9000000000200000;
	. = KERNEL_VMA;
	. = ALIGN(4096);
	boot_text_start_pa = .;
	.boot.text :
	{
		KEEP(*(.bootstrap))
		*(.bootstrap)
		*(.bootstrap.*)
		. = ALIGN(4096);
		*(.initial_pgtable_section)
		. = ALIGN(4096);
	}


	. = ALIGN(4096);
	text_start_pa = .;
	__executable_start = .;
	.text (text_start_pa): 
	{
		_text = .;

		/* any files' .text */
		*(.text)

		/* any files' .text.*, for example: rust .text._ZN* */
		*(.text.*)

		_etext = .;
		__etext = .;
	}
	. = ALIGN(32768);
	data_start_pa = .;
	.data (data_start_pa):
	{
		_data = .;
		*(.data)
		*(.data.*)
		*(.got.plt)
   		*(.got)
		_edata = .;
	}

	. = ALIGN(32768);

	rodata_start_pa = .;
	.rodata (rodata_start_pa):
	{
		_rodata = .;	
		*(.rodata)
		*(.rodata.*)
		*(.gcc_except_table .gcc_except_table.*)
		_erodata = .;
	}

	. = ALIGN(32768);
	
	trace_point_start_pa = .;
	.tracepoint (trace_point_start_pa): AT(trace_point_start_pa - KERNEL_VMA)
    {
        _tracepoint = .;
        *(.tracepoint)
        *(.tracepoint.*)
        _etracepoint = .;
    }
    . = ALIGN(32768);

	init_proc_union_start_pa = .;
	.data.init_proc_union (init_proc_union_start_pa):
	 { *(.data.init_proc_union) }

	. = ALIGN(32768);
	 bss_start_pa = .;
	.bss (bss_start_pa):
	{
		_bss = .;
		*(.bss)
		*(.bss.*)
		*(.sbss)
		*(.sbss.*)
		_ebss = .;
	}

	eh_frame = .;
	.eh_frame (eh_frame):
	{
		__eh_frame_hdr_start = .;
		*(.eh_frame_hdr)
		__eh_frame_hdr_end = .;
		__eh_frame_start = .;
		*(.eh_frame)
		*(.rela.eh_frame)
		__eh_frame_end = .;
	}

	_end = .;

	/DISCARD/ : {
		/* *(.eh_frame) */
		
	}
}
