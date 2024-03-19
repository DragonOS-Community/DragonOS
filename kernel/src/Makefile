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
ifeq ($(ARCH), x86_64)
    LDFLAGS_UNWIND = --eh-frame-hdr
    RUSTFLAGS_UNWIND = -Cforce-unwind-tables -Clink-arg=-Wl,eh_frame.ld 
endif
endif

RUSTFLAGS = $(RUSTFLAGS_UNWIND)

CFLAGS = $(GLOBAL_CFLAGS) -fno-pie $(CFLAGS_UNWIND) -I $(shell pwd) -I $(shell pwd)/include

ifeq ($(ARCH), x86_64)
	CFLAGS +=  -I $(shell pwd)/arch/x86_64/include
else ifeq ($(ARCH), riscv64)
	CFLAGS +=  -I $(shell pwd)/arch/riscv64/include -I $(shell pwd)/arch/riscv64/
endif

export ASFLAGS := --64

LD_LIST := ""


kernel_subdirs := common driver debug smp syscall libs time


kernel_rust:
	RUSTFLAGS="$(RUSTFLAGS)" cargo +nightly-2023-08-15 $(CARGO_ZBUILD) build --release --target $(TARGET_JSON)


all: kernel

# if x86_64
ifeq ($(ARCH), x86_64)
	$(MAKE) __link_x86_64_kernel
else ifeq ($(ARCH), riscv64)
	$(MAKE) __link_riscv64_kernel
endif
	
	@echo "Kernel Build Done."

ECHO:
	@echo "$@"

$(kernel_subdirs): ECHO

	$(MAKE) -C $@ all CFLAGS="$(CFLAGS)" ASFLAGS="$(ASFLAGS)"  kernel_root_path="$(shell pwd)"

kernel: $(kernel_subdirs) kernel_rust

__link_riscv64_kernel:
	@echo "Linking kernel..."
	$(LD) -b elf64-littleriscv -z muldefs $(LDFLAGS_UNWIND) -o kernel $(shell find . -name "*.o") ../target/riscv64imac-unknown-none-elf/release/libdragonos_kernel.a -T arch/riscv64/link.ld --no-relax
	$(OBJCOPY) -I elf64-littleriscv -O elf64-littleriscv -R ".eh_frame" kernel ../../bin/kernel/kernel.elf
	@rm kernel
	$(MAKE) __dragon_stub PAYLOAD_ELF="$(shell pwd)/../../bin/kernel/kernel.elf"



__link_x86_64_kernel:
	@echo "Linking kernel..."
	$(LD) -b elf64-x86-64 -z muldefs $(LDFLAGS_UNWIND) -o kernel $(shell find . -name "*.o") ../target/x86_64-unknown-none/release/libdragonos_kernel.a -T arch/x86_64/link.lds --no-relax
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
	$(LD) -b elf64-x86-64 -z muldefs $(LDFLAGS_UNWIND) -o kernel $(shell find . -name "*.o") ../target/x86_64-unknown-none/release/libdragonos_kernel.a ./debug/kallsyms.o  -T arch/x86_64/link.lds --no-relax
	@echo "Generating kernel ELF file..."
# 生成内核文件
ifeq ($(UNWIND_ENABLE), yes)
	$(OBJCOPY) -I elf64-x86-64 -O elf64-x86-64 kernel ../../bin/kernel/kernel.elf
else
	$(OBJCOPY) -I elf64-x86-64 -O elf64-x86-64 -R ".eh_frame" kernel ../../bin/kernel/kernel.elf
endif
	rm kernel

__dragon_stub:
	@echo "Linking dragon_stub..."
	@mkdir -p $(ROOT_PATH)/bin/sysroot
	PAYLOAD_ELF=$(PAYLOAD_ELF) TARGET_SYSROOT=$(ROOT_PATH)/bin/sysroot $(MAKE) -C $(ROOT_PATH)/kernel/submodules/DragonStub install -j $(NPROCS)

	
clean: 
	@cargo clean
	rm -rf $(GARBAGE)
	@list='$(kernel_subdirs)'; for subdir in $$list; do \
		echo "Clean in dir: $$subdir";\
		cd $$subdir && $(MAKE) clean;\
		cd .. ;\
	done
