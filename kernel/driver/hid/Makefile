
CFLAGS += -I .

kernel_driver_hid_subdirs:= usbhid

kernel_driver_hid_objs:= $(shell find ./*.c)

ECHO:
	@echo "$@"

$(kernel_driver_hid_subdirs): ECHO
	$(MAKE) -C $@ all CFLAGS="$(CFLAGS)" ASFLAGS="$(ASFLAGS)" PIC="$(PIC)"

$(kernel_driver_hid_objs): ECHO
	$(CC) $(CFLAGS) -c $@ -o $@.o

all: $(kernel_driver_hid_objs) $(kernel_driver_hid_subdirs)
	@echo $(kernel_driver_hid_objs)

