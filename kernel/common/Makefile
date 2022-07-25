
CFLAGS += -I .

kernel_common_subdirs:=libELF math

all: glib.o printk.o cpu.o
	@list='$(kernel_common_subdirs)'; for subdir in $$list; do \
    		echo "make all in $$subdir";\
    		cd $$subdir;\
    		$(MAKE) all CFLAGS="$(CFLAGS)";\
    		cd ..;\
	done

glib.o: glib.c
	gcc $(CFLAGS) -c glib.c -o glib.o

printk.o: printk.c
	gcc $(CFLAGS) -c printk.c -o printk.o

cpu.o: cpu.c
	gcc $(CFLAGS) -c cpu.c -o cpu.o