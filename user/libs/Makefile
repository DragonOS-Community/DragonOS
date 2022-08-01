
user_libs_sub_dirs=libc libsystem libKeyboard


ECHO:
	@echo "$@"

$(user_libs_sub_dirs): ECHO

	$(MAKE) -C $@ all CFLAGS="$(CFLAGS) -I $(shell pwd)"

all: $(user_libs_sub_dirs)
