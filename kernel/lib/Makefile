
CFLAGS += -I .

kernel_lib_subdirs:= libUI

ECHO:
	@echo "$@"

$(kernel_lib_subdirs): ECHO
	$(MAKE) -C $@ all CFLAGS="$(CFLAGS)" ASFLAGS="$(ASFLAGS)" PIC="$(PIC)"

all: $(kernel_lib_subdirs)
