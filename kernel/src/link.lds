
OUTPUT_FORMAT("elf64-x86-64","elf64-x86-64","elf64-x86-64")
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)

SECTIONS
{
	KERNEL_VMA = 0xffff800000000000;
	//KERNEL_VMA = 0;
	. = 0;
	. = 0x100000;
	
	.boot.text :
	{
		KEEP(*(.multiboot_header))
		head.o(.bootstrap)
		head.o(.bootstrap.code64)
		head.o(.bootstrap.data)
		. = ALIGN(4096);
	}
	
	. += KERNEL_VMA;
	. = ALIGN(32768);
	text_start_pa = .;
	.text (text_start_pa): AT(text_start_pa - KERNEL_VMA)
	{
		_text = .;
		
		*(.text)
		
		_etext = .;
	}
	. = ALIGN(32768);
	data_start_pa = .;
	.data (data_start_pa): AT(data_start_pa - KERNEL_VMA)
	{
		_data = .;
		*(.data)
		
		_edata = .;
	}

	. = ALIGN(32768);

	rodata_start_pa = .;
	.rodata (rodata_start_pa): AT(rodata_start_pa - KERNEL_VMA)
	{
		_rodata = .;	
		*(.rodata)
		*(.rodata.*)
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

	_end = .;

	/DISCARD/ : {
		*(.eh_frame)
		
	}
}