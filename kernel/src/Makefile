SUBDIR_ROOTS := .
DIRS := . $(shell find $(SUBDIR_ROOTS) -type d)
GARBAGE_PATTERNS := *.o *.s~ *.s *.S~ *.c~ *.h~ kernel
GARBAGE := $(foreach DIR,$(DIRS),$(addprefix $(DIR)/,$(GARBAGE_PATTERNS)))

DIR_LIB=libs
lib_patterns := *.a
LIB_FILES := $(foreach DIR,$(DIR_LIB),$(addprefix $(DIR)/,$(lib_patterns)))


# unwind/backtrace related
UNWIND_ENABLE ?= yes
CFLAGS_UNWIND =
LDFLAGS_UNWIND =
RUSTFLAGS_UNWIND =
ifeq ($(UNWIND_ENABLE), yes)
    CFLAGS_UNWIND = -funwind-tables
    LDFLAGS_UNWIND = --eh-frame-hdr
    RUSTFLAGS_UNWIND = -Cforce-unwind-tables -Clink-arg=-Wl,eh_frame.ld 
endif

CFLAGS = $(GLOBAL_CFLAGS) -fno-pie $(CFLAGS_UNWIND) -I $(shell pwd) -I $(shell pwd)/include -I $(shell pwd)/arch/x86_64/include

export ASFLAGS := --64

LD_LIST := ""


kernel_subdirs := common driver debug arch exception smp syscall ktest libs time


main.o: main.c 
# -fno-builtin: 不使用C语言内建函数
# The -m64 option sets int to 32bits and long and pointer to 64 bits and generates code for AMD’s x86-64 architecture.
	$(CC) $(CFLAGS) -c main.c  -o main.o

kernel_rust:
	RUSTFLAGS="$(RUSTFLAGS_UNWIND)" cargo +nightly-2023-01-21 build --release --target ./arch/x86_64/x86_64-unknown-none.json

all: kernel

	@echo "Linking kernel..."
	$(LD) -b elf64-x86-64 -z muldefs $(LDFLAGS_UNWIND) -o kernel main.o $(shell find . -name "*.o") ../target/x86_64-unknown-none/release/libdragonos_kernel.a -T link.lds --no-relax
# 生成kallsyms
	current_dir=$(pwd)
	
	@dbg='debug';for x in $$dbg; do \
		cd $$x;\
		$(MAKE) generate_kallsyms kernel_root_path="$(shell pwd)"||exit 1;\
		cd ..;\
	done
	
	
# 重新链接
	@echo "Re-Linking kernel..."
	@echo $(shell find . -name "*.o")
	$(LD) -b elf64-x86-64 -z muldefs $(LDFLAGS_UNWIND) -o kernel main.o $(shell find . -name "*.o") ../target/x86_64-unknown-none/release/libdragonos_kernel.a ./debug/kallsyms.o  -T link.lds --no-relax
	@echo "Generating kernel ELF file..."
# 生成内核文件
ifeq ($(UNWIND_ENABLE), yes)
	$(OBJCOPY) -I elf64-x86-64 -O elf64-x86-64 kernel ../../bin/kernel/kernel.elf
else
	$(OBJCOPY) -I elf64-x86-64 -O elf64-x86-64 -R ".eh_frame" kernel ../../bin/kernel/kernel.elf
endif
	@echo "Kernel Build Done."

ECHO:
	@echo "$@"

$(kernel_subdirs): ECHO

	$(MAKE) -C $@ all CFLAGS="$(CFLAGS)" ASFLAGS="$(ASFLAGS)"  kernel_root_path="$(shell pwd)"

kernel: main.o $(kernel_subdirs) kernel_rust
	


clean: 
	cargo clean
	rm -rf $(GARBAGE)
	@list='$(kernel_subdirs)'; for subdir in $$list; do \
		echo "Clean in dir: $$subdir";\
		cd $$subdir && $(MAKE) clean;\
		cd .. ;\
	done
