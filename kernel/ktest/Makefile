
CFLAGS += -I .

kernel_ktest_objs:= $(shell find ./*.c)

ECHO:
	@echo "$@"

$(kernel_ktest_objs): ECHO
	gcc $(CFLAGS) -c $@ -o $@.o

all: $(kernel_ktest_objs)

