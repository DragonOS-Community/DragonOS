
CFLAGS += -I .

ifeq ($(ARCH), __x86_64__)
kernel_arch_subdirs:=x86_64
endif

all: 
	@list='$(kernel_arch_subdirs)'; for subdir in $$list; do \
    		echo "make all in $$subdir";\
    		cd $$subdir;\
    		$(MAKE) all CFLAGS="$(CFLAGS)" ;\
    		cd ..;\
	done

clean:
	echo "Done."