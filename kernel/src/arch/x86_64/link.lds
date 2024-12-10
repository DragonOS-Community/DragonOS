
OUTPUT_FORMAT("elf64-x86-64","elf64-x86-64","elf64-x86-64")
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)

SECTIONS
{
	KERNEL_VMA = 0xffff800000000000;
	//KERNEL_VMA = 0;
	. = 0;
	. = 0x100000;
	_default_kernel_load_base = .;
	.boot.text :
	{
		KEEP(*(.multiboot_header))
		KEEP(*(.multiboot2_header))
		
		*(.bootstrap)
		*(.bootstrap.code64)
		*(.bootstrap.data)
		
		. = ALIGN(4096);
	}


	. = ALIGN(32768);
	. += KERNEL_VMA;
	text_start_pa = .;
	__executable_start = .;
	.text (text_start_pa): AT(text_start_pa - KERNEL_VMA)
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
	.data (data_start_pa): AT(data_start_pa - KERNEL_VMA)
	{
		_data = .;
		*(.data)
		*(.data.*)
		
		_edata = .;
	}

	. = ALIGN(32768);

	rodata_start_pa = .;
	.rodata (rodata_start_pa): AT(rodata_start_pa - KERNEL_VMA)
	{
		_rodata = .;	
		*(.rodata)
		*(.rodata.*)
		*(.note.gnu.*)
		*(.fixup)
		*(.gcc_except_table .gcc_except_table.*)
		_erodata = .;
	}

	. = ALIGN(32768);

	init_proc_union_start_pa = .;
	.data.init_proc_union (init_proc_union_start_pa): AT(init_proc_union_start_pa - KERNEL_VMA)
	 { *(.data.init_proc_union) }

	. = ALIGN(32768);
	 bss_start_pa = .;
	.bss (bss_start_pa): AT(bss_start_pa - KERNEL_VMA)
	{
		_bss = .;
		*(.bss)
		*(.bss.*)
		_ebss = .;
	}

	eh_frame = .;
	.eh_frame (eh_frame): AT(eh_frame - KERNEL_VMA)
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
