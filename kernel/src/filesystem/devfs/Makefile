
CFLAGS += -I .


kernel_fs_devfs_objs:= $(shell find ./*.c)


ECHO:
	@echo "$@"


$(kernel_fs_devfs_objs): ECHO
	$(CC) $(CFLAGS) -c $@ -o $@.o


all: $(kernel_fs_devfs_objs)

