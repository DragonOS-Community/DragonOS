
CFLAGS += -I .

kernel_common_subdirs:=libELF

all: 
	@list='$(kernel_common_subdirs)'; for subdir in $$list; do \
    		echo "make all in $$subdir";\
    		cd $$subdir;\
    		$(MAKE) all CFLAGS="$(CFLAGS) -I";\
    		cd ..;\
	done
