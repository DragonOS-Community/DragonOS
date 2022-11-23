
CFLAGS += -I .

kernel_arch_x86_64_subdirs:= asm

kernel_arch_x86_64_objs:= $(shell find ./*.c)

ECHO:
	@echo "$@"


$(kernel_arch_x86_64_objs): ECHO
	$(CC) $(CFLAGS) -c $@ -o $@.o

$(kernel_arch_x86_64_subdirs): ECHO
	$(MAKE) -C $@ all CFLAGS="$(CFLAGS)" ASFLAGS="$(ASFLAGS)" PIC="$(PIC)"

all: $(kernel_arch_x86_64_objs) $(kernel_arch_x86_64_subdirs)


clean:
	echo "Done."