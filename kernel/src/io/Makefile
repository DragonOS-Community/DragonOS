
CFLAGS += -I .

kernel_io_subdirs:=block

ECHO:
	@echo "$@"

$(kernel_io_subdirs): ECHO

	$(MAKE) -C $@ all CFLAGS="$(CFLAGS)" ASFLAGS="$(ASFLAGS)" PIC="$(PIC)"

all: $(kernel_io_subdirs)


clean:
	echo "Done."