
CFLAGS += -I .

kernel_common_subdirs:=libELF math

ECHO:
	@echo "$@"

$(kernel_common_subdirs): ECHO

	$(MAKE) -C $@ all CFLAGS="$(CFLAGS)" ASFLAGS="$(ASFLAGS)" PIC="$(PIC)"

all: glib.o printk.o cpu.o bitree.o kfifo.o wait_queue.o mutex.o wait.o unistd.o string.o semaphore.o $(kernel_common_subdirs)


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

unistd.o: unistd.c
	gcc $(CFLAGS) -c unistd.c -o unistd.o

string.o: string.c
	gcc $(CFLAGS) -c string.c -o string.o

semaphore.o: semaphore.c
	gcc $(CFLAGS) -c semaphore.c -o semaphore.o