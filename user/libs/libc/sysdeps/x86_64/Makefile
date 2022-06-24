

all: start.o

ifeq ($(ARCH), __x86_64__)
start.o:
	gcc $(CFLAGS) -c elf/start.c -o elf/start.o
endif

clean:
	
	echo "Done."