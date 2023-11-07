
CFLAGS += -I .

kernel_driver_subdirs:=pci acpi disk keyboard mouse multiboot2 timers hid 

ECHO:
	@echo "$@"

$(kernel_driver_subdirs): ECHO

	$(MAKE) -C $@ all CFLAGS="$(CFLAGS)" ASFLAGS="$(ASFLAGS)" 

all: $(kernel_driver_subdirs)


clean:
	echo "Done."
