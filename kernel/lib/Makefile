
CFLAGS += -I .

kernel_lib_subdirs:= libUI

kernel_lib_objs:= $(shell find ./*.c)

ECHO:
	@echo "$@"

$(kernel_lib_subdirs): ECHO
	$(MAKE) -C $@ all CFLAGS="$(CFLAGS)" ASFLAGS="$(ASFLAGS)" PIC="$(PIC)"

$(kernel_lib_objs): ECHO
	gcc $(CFLAGS) -c $@ -o $@.o

all: $(kernel_lib_objs) $(kernel_lib_subdirs)
	@echo $(kernel_lib_objs)

