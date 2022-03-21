SUBDIR_ROOTS := . common
DIRS := . $(shell find $(SUBDIR_ROOTS) -type d)
GARBAGE_PATTERNS := *.o *.s~ *.s *.S~ *.c~ *.h~ kernel 
GARBAGE := $(foreach DIR,$(DIRS),$(addprefix $(DIR)/,$(GARBAGE_PATTERNS)))

DIR_LIB=lib
lib_patterns := *.a
LIB_FILES := $(foreach DIR,$(DIR_LIB),$(addprefix $(DIR)/,$(lib_patterns)))

CFLAGS := -mcmodel=large -fno-builtin -m64

# 控制操作系统使用的中断控制器 _INTR_8259A_ _INTR_APIC_
PIC := _INTR_APIC_
CFLAGS += -D $(PIC)

ASFLAGS := --64

all: kernel
	objcopy -I elf64-x86-64 -S -R ".comment" -R ".eh_frame" -O elf64-x86-64 kernel ../bin/kernel/kernel.elf


kernel: head.o entry.o main.o printk.o trap.o mm.o slab.o irq.o pic.o process.o syscall.o multiboot2.o cpu.o acpi.o ps2_keyboard.o ps2_mouse.o ata.o
	ld -b elf64-x86-64 -z muldefs -o kernel head.o exception/entry.o main.o common/printk.o exception/trap.o exception/irq.o mm/mm.o mm/slab.o process/process.o syscall/syscall.o driver/multiboot2/multiboot2.o \
	common/cpu.o	\
	driver/acpi/acpi.o driver/interrupt/pic.o driver/keyboard/ps2_keyboard.o driver/mouse/ps2_mouse.o driver/disk/ata.o \
	-T link.lds

head.o: head.S
	gcc -E head.S > head.s # 预处理
	as $(ASFLAGS) -o head.o head.s
#gcc -mcmodel=large -fno-builtin -m64 -c head.S -o head.o

entry.o: exception/entry.S
	gcc -E exception/entry.S > exception/entry.s
	as $(ASFLAGS) -o exception/entry.o exception/entry.s



main.o: main.c 
# -fno-builtin: 不使用C语言内建函数
# The -m64 option sets int to 32bits and long and pointer to 64 bits and generates code for AMD’s x86-64 architecture.
	gcc $(CFLAGS) -c main.c  -o main.o


printk.o: common/printk.c
	gcc $(CFLAGS) -c common/printk.c -o common/printk.o

trap.o:	exception/trap.c
	gcc $(CFLAGS) -c exception/trap.c -o exception/trap.o

irq.o: exception/irq.c
	gcc $(CFLAGS) -c exception/irq.c -o exception/irq.o



mm.o: mm/mm.c
	gcc $(CFLAGS) -c mm/mm.c -o mm/mm.o

slab.o: mm/slab.c
	gcc $(CFLAGS) -c mm/slab.c -o mm/slab.o

process.o: process/process.c
	gcc $(CFLAGS) -c process/process.c -o process/process.o
syscall.o: syscall/syscall.c
	gcc $(CFLAGS) -c syscall/syscall.c -o syscall/syscall.o



cpu.o: common/cpu.c 
	gcc $(CFLAGS) -c common/cpu.c -o common/cpu.o

# 驱动程序
# 中断处理芯片的驱动程序
ifeq ($(PIC), _INTR_8259A_)
pic.o: driver/interrupt/8259A/8259A.c
	gcc $(CFLAGS) -c driver/interrupt/8259A/8259A.c -o driver/interrupt/pic.o
else
pic.o: driver/interrupt/apic/apic.c
	gcc $(CFLAGS) -c driver/interrupt/apic/apic.c -o driver/interrupt/pic.o
endif

multiboot2.o: driver/multiboot2/multiboot2.c 
	gcc $(CFLAGS) -c driver/multiboot2/multiboot2.c  -o driver/multiboot2/multiboot2.o

acpi.o: driver/acpi/acpi.c 
	gcc $(CFLAGS) -c driver/acpi/acpi.c  -o driver/acpi/acpi.o

ps2_keyboard.o: driver/keyboard/ps2_keyboard.c
	gcc $(CFLAGS) -c driver/keyboard/ps2_keyboard.c  -o driver/keyboard/ps2_keyboard.o

ps2_mouse.o: driver/mouse/ps2_mouse.c
	gcc $(CFLAGS) -c driver/mouse/ps2_mouse.c -o driver/mouse/ps2_mouse.o

ata.o: driver/disk/ata.c
	gcc $(CFLAGS) -c driver/disk/ata.c -o driver/disk/ata.o


clean: 
	rm -rf $(GARBAGE)