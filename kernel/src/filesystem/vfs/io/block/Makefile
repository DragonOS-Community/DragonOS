
CFLAGS += -I .


kernel_io_block_objs:= $(shell find ./*.c )

ECHO:
	@echo "$@"

$(kernel_io_block_objs): ECHO
	$(CC) $(CFLAGS) -c $@ -o $@.o

all: $(kernel_io_block_objs)


clean:
	echo "Done."