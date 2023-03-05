
user_libs_sub_dirs=libc libsystem libKeyboard


ECHO:
	@echo "$@"

$(user_libs_sub_dirs): ECHO

	$(MAKE) -C $@ all CFLAGS="$(CFLAGS) -I $(shell pwd)"

all: $(user_libs_sub_dirs)

clean:
	@list='$(user_libs_sub_dirs)'; for subdir in $$list; do \
		echo "Clean in dir: $$subdir";\
		cd $$subdir && $(MAKE) clean;\
		cd .. ;\
	done