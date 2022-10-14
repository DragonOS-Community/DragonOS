
CFLAGS += -I .


kernel_driver_usbhid_objs:= $(shell find ./*.c)

ECHO:
	@echo "$@"

$(kernel_driver_usbhid_objs): ECHO
	gcc $(CFLAGS) -c $@ -o $@.o

all: $(kernel_driver_hid_objs) $(kernel_driver_hid_subdirs)
	@echo $(kernel_driver_hid_objs)

