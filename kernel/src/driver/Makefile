
CFLAGS += -I .

kernel_driver_subdirs:=video interrupt usb pci acpi disk keyboard mouse multiboot2 timers hid virtio

ECHO:
	@echo "$@"

$(kernel_driver_subdirs): ECHO

	$(MAKE) -C $@ all CFLAGS="$(CFLAGS)" ASFLAGS="$(ASFLAGS)" PIC="$(PIC)"

all: $(kernel_driver_subdirs)


clean:
	echo "Done."
