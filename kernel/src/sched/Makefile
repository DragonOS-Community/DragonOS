
CFLAGS += -I .


kernel_sched_objs:= $(shell find ./*.c)

ECHO:
	@echo "$@"


$(kernel_sched_objs): ECHO
	$(CC) $(CFLAGS) -c $@ -o $@.o

all: $(kernel_sched_objs)


clean:
	echo "Done."