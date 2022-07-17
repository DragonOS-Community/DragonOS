
CFLAGS += -I .

kernel_driver_subdirs:=video interrupt usb

all: 
	@list='$(kernel_driver_subdirs)'; for subdir in $$list; do \
    		echo "make all in $$subdir";\
    		cd $$subdir;\
    		$(MAKE) all CFLAGS="$(CFLAGS)" PIC="$(PIC)";\
    		cd ..;\
	done

clean:
	echo "Done."