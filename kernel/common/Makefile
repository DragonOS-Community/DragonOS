
CFLAGS += -I .

kernel_common_subdirs:=libELF math

all: glib.o printk.o cpu.o bitree.o kfifo.o wait_queue.o mutex.o wait.o
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

bitree.o: bitree.c
	gcc $(CFLAGS) -c bitree.c -o bitree.o

kfifo.o: kfifo.c
	gcc $(CFLAGS) -c kfifo.c -o kfifo.o

wait_queue.o: wait_queue.c
	gcc $(CFLAGS) -c wait_queue.c -o wait_queue.o

mutex.o: mutex.c
	gcc $(CFLAGS) -c mutex.c -o mutex.o

wait.o: sys/wait.c
	gcc $(CFLAGS) -c sys/wait.c -o sys/wait.o