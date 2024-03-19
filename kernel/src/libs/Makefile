
CFLAGS += -I .

kernel_lib_subdirs:=

kernel_lib_objs:= $(shell find ./*.c)

ECHO:
	@echo "$@"

$(kernel_lib_subdirs): ECHO
	$(MAKE) -C $@ all CFLAGS="$(CFLAGS)" ASFLAGS="$(ASFLAGS)" 

$(kernel_lib_objs): ECHO
	$(CC) $(CFLAGS) -c $@ -o $@.o

all: $(kernel_lib_objs) $(kernel_lib_subdirs)
	@echo $(kernel_lib_objs)

