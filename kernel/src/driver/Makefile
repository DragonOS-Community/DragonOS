
CFLAGS += -I .

kernel_driver_subdirs:=acpi disk multiboot2 timers hid 

ECHO:
	@echo "$@"

$(kernel_driver_subdirs): ECHO

	$(MAKE) -C $@ all CFLAGS="$(CFLAGS)" ASFLAGS="$(ASFLAGS)" 

all: $(kernel_driver_subdirs)


clean:
	echo "Done."
