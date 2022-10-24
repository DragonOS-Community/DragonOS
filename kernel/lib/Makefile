
CFLAGS += -I .

kernel_lib_subdirs:= libUI sys libELF rust_helloworld

kernel_lib_objs:= $(shell find ./*.c)

ECHO:
	@echo "$@"

$(kernel_lib_subdirs): ECHO
	$(MAKE) -C $@ all CFLAGS="$(CFLAGS)" ASFLAGS="$(ASFLAGS)" PIC="$(PIC)"

$(kernel_lib_objs): ECHO
	$(CC) $(CFLAGS) -c $@ -o $@.o

all: $(kernel_lib_objs) $(kernel_lib_subdirs)
	@echo $(kernel_lib_objs)

