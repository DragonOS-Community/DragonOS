
OUTPUT_FORMAT("elf64-x86-64","elf64-x86-64","elf64-x86-64")
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)

SECTIONS
{

	. = 0x800000;
	
	
	.text :
	{
		_text = .;
		
		*(.text)
		
		_etext = .;
	}
	. = ALIGN(8);
	
	.data :
	{
		_data = .;
		*(.data)
		
		_edata = .;
	}


	rodata_start_pa = .;
	.rodata :
	{
		_rodata = .;	
		*(.rodata)
		_erodata = .;
	}

	
	.bss :
	{
		_bss = .;
		*(.bss)
		_ebss = .;
	}

	_end = .;


}