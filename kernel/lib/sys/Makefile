
CFLAGS += -I .

kernel_lib_sys_objs:= $(shell find ./*.c)

ECHO:
	@echo "$@"

$(kernel_lib_sys_objs): ECHO
	$(MAKE) -C $@ all CFLAGS="$(CFLAGS)" ASFLAGS="$(ASFLAGS)" PIC="$(PIC)"

$(kernel_lib_sys_objs): ECHO
	gcc $(CFLAGS) -c $@ -o $@.o

all: $(kernel_lib_sys_objs) $(kernel_lib_sys_objs)
	@echo $(kernel_lib_sys_objs)

