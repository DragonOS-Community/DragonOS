OUTPUT_FORMAT(
    "elf64-littleriscv",
    "elf64-littleriscv",
	"elf64-littleriscv"
)

OUTPUT_ARCH(riscv)
ENTRY(_start)


SECTIONS
{
	//KERNEL_VMA = 0xffffffc000000000;
	KERNEL_VMA = 0;
	. = 0;
	
	.boot.text :
	{
		KEEP(*(.multiboot_header))
		*(.bootstrap)
		*(.bootstrap.code64)
		*(.bootstrap.data)
		. = ALIGN(4096);
	}

	. += KERNEL_VMA;
	. = ALIGN(32768);
	text_start_pa = .;
	.text (text_start_pa): AT(text_start_pa - KERNEL_VMA)
	{
		_text = .;

		/* any files' .text */
		*(.text)

		/* any files' .text.*, for example: rust .text._ZN* */
		*(.text.*)

		_etext = .;
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
