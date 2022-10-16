
CFLAGS += -I .


kernel_fs_objs:= $(shell find ./*.c)
kernel_fs_subdirs:= devfs VFS fat32 rootfs

ECHO:
	@echo "$@"


$(kernel_fs_objs): ECHO
	$(CC) $(CFLAGS) -c $@ -o $@.o

$(kernel_fs_subdirs): ECHO
	$(MAKE) -C $@ all CFLAGS="$(CFLAGS)" ASFLAGS="$(ASFLAGS)" PIC="$(PIC)"


all: $(kernel_fs_objs) $(kernel_fs_subdirs)

