
user_libs_sub_dirs=libc libsystem


all:
	@list='$(user_libs_sub_dirs)'; for subdir in $$list; do \
    		echo "make all in $$subdir";\
    		cd $$subdir;\
    		$(MAKE) all CFLAGS="$(CFLAGS) -I $(shell pwd)";\
    		cd ..;\
	done