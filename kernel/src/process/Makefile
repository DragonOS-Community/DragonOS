
CFLAGS += -I .


kernel_process_objs:= $(shell find ./*.c)

ECHO:
	@echo "$@"


$(kernel_process_objs): ECHO
	$(CC) $(CFLAGS) -c $@ -o $@.o

all: $(kernel_process_objs)


clean:
	echo "Done."