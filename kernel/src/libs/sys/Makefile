
CFLAGS += -I .

kernel_lib_sys_objs:= $(shell find ./*.c)

ECHO:
	@echo "$@"

$(kernel_lib_sys_objs): ECHO
	$(CC) $(CFLAGS) -c $@ -o $@.o

all: $(kernel_lib_sys_objs)
	@echo $(kernel_lib_sys_objs)

