SUBDIR_ROOTS := .
DIRS := . $(shell find $(SUBDIR_ROOTS) -type d)
GARBAGE_PATTERNS := *.o *.s~ *.s *.S~ *.c~ *.h~ kernel
GARBAGE := $(foreach DIR,$(DIRS),$(addprefix $(DIR)/,$(GARBAGE_PATTERNS)))

DIR_LIB=libs
lib_patterns := *.a
LIB_FILES := $(foreach DIR,$(DIR_LIB),$(addprefix $(DIR)/,$(lib_patterns)))


# 控制操作系统使用的中断控制器 _INTR_8259A_ _INTR_APIC_
PIC := _INTR_APIC_
CFLAGS = $(GLOBAL_CFLAGS) -D $(PIC) -I $(shell pwd) -I $(shell pwd)/include -I $(shell pwd)/arch/x86_64/include

export ASFLAGS := --64

LD_LIST := head.o


kernel_subdirs := common driver process debug time arch exception mm smp sched syscall ktest libs ipc io
	


head.o: head.S
	$(CC) -E head.S > _head.s # 预处理
	$(AS) $(ASFLAGS) -o head.o _head.s


main.o: main.c 
# -fno-builtin: 不使用C语言内建函数
# The -m64 option sets int to 32bits and long and pointer to 64 bits and generates code for AMD’s x86-64 architecture.
	$(CC) $(CFLAGS) -c main.c  -o main.o

kernel_rust:
	rustup default nightly
	cargo +nightly build --release --target ./arch/x86_64/x86_64-unknown-none.json

all: kernel

	@echo "Linking kernel..."
	$(LD) -b elf64-x86-64 -z muldefs -o kernel head.o main.o $(shell find . -name "*.o") ../target/x86_64-unknown-none/release/libdragonos_kernel.a -T link.lds
# 生成kallsyms
	current_dir=$(pwd)
	
	@dbg='debug';for x in $$dbg; do \
		cd $$x;\
		$(MAKE) generate_kallsyms kernel_root_path="$(shell pwd)";\
		cd ..;\
		if [ "$$?" != "0" ]; then\
            exit $$?;\
        fi;\
	done
	
	
# 重新链接
	@echo "Re-Linking kernel..."
	@echo $(shell find . -name "*.o")
	$(LD) -b elf64-x86-64 -z muldefs -o kernel head.o main.o $(shell find . -name "*.o") ../target/x86_64-unknown-none/release/libdragonos_kernel.a ./debug/kallsyms.o  -T link.lds
	@echo "Generating kernel ELF file..."
# 生成内核文件
	$(OBJCOPY) -I elf64-x86-64 -O elf64-x86-64 -R ".comment" -R ".eh_frame" kernel ../../bin/kernel/kernel.elf
	@echo "Kernel Build Done."

ECHO:
	@echo "$@"

$(kernel_subdirs): ECHO

	$(MAKE) -C $@ all CFLAGS="$(CFLAGS)" ASFLAGS="$(ASFLAGS)" PIC="$(PIC)" kernel_root_path="$(shell pwd)"

kernel: head.o main.o $(kernel_subdirs) kernel_rust
	


clean: 
	cargo clean
	rm -rf $(GARBAGE)
	@list='$(kernel_subdirs)'; for subdir in $$list; do \
		echo "Clean in dir: $$subdir";\
		cd $$subdir && $(MAKE) clean;\
		cd .. ;\
	done