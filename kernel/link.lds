
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
	.text : AT(ADDR(.text) - KERNEL_VMA)
	{
		_text = .;
		
		*(.text)
		_etext = .;
	}
	. = ALIGN(8);
	.data : AT(ADDR(.data) - KERNEL_VMA)
	{
		_data = .;
		*(.data)
		
		_edata = .;
	}
	.rodata : AT(ADDR(.rodata) - KERNEL_VMA)
	{
		_rodata = .;	
		*(.rodata)
		_erodata = .;
	}

	. = ALIGN(32768);
	.data.init_proc_union : AT(ADDR(.data.init_proc_union) - KERNEL_VMA)
	 { *(.data.init_proc_union) }
	.bss : AT(ADDR(.bss) - KERNEL_VMA)
	{
		_bss = .;
		*(.bss)
		_ebss = .;
	}

	_end = .;
}