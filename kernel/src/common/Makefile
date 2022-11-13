
CFLAGS += -I .

kernel_common_subdirs:= math

ECHO:
	@echo "$@"

$(kernel_common_subdirs): ECHO

	$(MAKE) -C $@ all CFLAGS="$(CFLAGS)" ASFLAGS="$(ASFLAGS)" PIC="$(PIC)"

all:  $(kernel_common_subdirs)

