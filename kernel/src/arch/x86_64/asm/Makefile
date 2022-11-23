
CFLAGS += -I .

# kernel_arch_x86_64_asm_subdirs:= 

kernel_arch_x86_64_asm_objs:= $(shell find ./*.c)

ECHO:
	@echo "$@"


$(kernel_arch_x86_64_asm_objs): ECHO
	$(CC) $(CFLAGS) -c $@ -o $@.o

# $(kernel_arch_x86_64_asm_subdirs): ECHO
# 	$(MAKE) -C $@ all CFLAGS="$(CFLAGS)" ASFLAGS="$(ASFLAGS)" PIC="$(PIC)"

all: $(kernel_arch_x86_64_asm_objs)


clean:
	echo "Done."