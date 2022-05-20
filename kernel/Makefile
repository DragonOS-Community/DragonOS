SUBDIR_ROOTS := .
DIRS := . $(shell find $(SUBDIR_ROOTS) -type d)
GARBAGE_PATTERNS := *.o *.s~ *.s *.S~ *.c~ *.h~ kernel
GARBAGE := $(foreach DIR,$(DIRS),$(addprefix $(DIR)/,$(GARBAGE_PATTERNS)))

DIR_LIB=lib
lib_patterns := *.a
LIB_FILES := $(foreach DIR,$(DIR_LIB),$(addprefix $(DIR)/,$(lib_patterns)))


# 控制操作系统使用的中断控制器 _INTR_8259A_ _INTR_APIC_
PIC := _INTR_APIC_
CFLAGS = $(GLOBAL_CFLAGS) -D $(PIC) -I $(shell pwd)

ASFLAGS := --64

LD_LIST := head.o
OBJ_LIST := head.o


kernel_subdirs := common driver process
	




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


sched.o: sched/sched.c
	gcc $(CFLAGS) -c sched/sched.c -o sched/sched.o

syscall.o: syscall/syscall.c
	gcc $(CFLAGS) -c syscall/syscall.c -o syscall/syscall.o

smp.o: smp/smp.c 
	gcc $(CFLAGS) -c smp/smp.c  -o smp/smp.o 

apu_boot.o: smp/apu_boot.S
	gcc -E smp/apu_boot.S > smp/apu_boot.s # 预处理
	as $(ASFLAGS) -o smp/apu_boot.o smp/apu_boot.s

cpu.o: common/cpu.c 
	gcc $(CFLAGS) -c common/cpu.c -o common/cpu.o

softirq.o: exception/softirq.c
	gcc $(CFLAGS) -c exception/softirq.c -o exception/softirq.o

fat32.o: filesystem/fat32/fat32.c
	gcc $(CFLAGS) -c filesystem/fat32/fat32.c -o filesystem/fat32/fat32.o

MBR.o: filesystem/MBR.c
	gcc $(CFLAGS) -c filesystem/MBR.c -o filesystem/MBR.o

VFS.o: filesystem/VFS/VFS.c
	gcc $(CFLAGS) -c filesystem/VFS/VFS.c -o filesystem/VFS/VFS.o

# IPI的代码
ifeq ($(ARCH), __x86_64__)
OBJ_LIST += ipi.o
LD_LIST += arch/x86_64/x86_64_ipi.o
ipi.o: arch/x86_64/x86_64_ipi.c
	gcc $(CFLAGS) -c arch/x86_64/x86_64_ipi.c -o arch/x86_64/x86_64_ipi.o
	
endif

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

pci.o: driver/pci/pci.c
	gcc $(CFLAGS) -c driver/pci/pci.c -o driver/pci/pci.o

ahci.o: driver/disk/ahci/ahci.c
	gcc $(CFLAGS) -c driver/disk/ahci/ahci.c -o driver/disk/ahci/ahci.o

rtc.o: driver/timers/rtc/rtc.c
	gcc $(CFLAGS) -c driver/timers/rtc/rtc.c -o driver/timers/rtc/rtc.o

HPET.o: driver/timers/HPET/HPET.c
	gcc $(CFLAGS) -c driver/timers/HPET/HPET.c -o driver/timers/HPET/HPET.o

timer.o: driver/timers/timer.c
	gcc $(CFLAGS) -c driver/timers/timer.c -o driver/timers/timer.o

OBJ_LIST += uart.o
LD_LIST += driver/uart/uart.o
uart.o: driver/uart/uart.c
	gcc $(CFLAGS) -c driver/uart/uart.c -o driver/uart/uart.o


all: kernel

	ld -b elf64-x86-64 -z muldefs -o kernel head.o main.o $(shell find . -name "*.o") -T link.lds
	objcopy -I elf64-x86-64 -O elf64-x86-64 -R ".comment" -R ".eh_frame" kernel ../bin/kernel/kernel.elf

kernel: head.o entry.o main.o printk.o trap.o mm.o slab.o irq.o pic.o sched.o syscall.o multiboot2.o cpu.o acpi.o ps2_keyboard.o ps2_mouse.o ata.o pci.o ahci.o smp.o apu_boot.o rtc.o HPET.o softirq.o timer.o fat32.o MBR.o VFS.o $(OBJ_LIST)
	
	@list='$(kernel_subdirs)'; for subdir in $$list; do \
    		echo "make all in $$subdir";\
    		cd $$subdir;\
    		$(MAKE) all CFLAGS="$(CFLAGS)" ASFLAGS="$(ASFLAGS)";\
    		cd ..;\
	done

	


clean: 
	rm -rf $(GARBAGE)