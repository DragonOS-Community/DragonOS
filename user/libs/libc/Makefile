all: libc

CFLAGS += -I .

libc_sub_dirs=math sys

ifeq ($(ARCH), __x86_64__)
libc_sub_dirs += sysdeps/x86_64
endif


libc: unistd.o fcntl.o malloc.o errno.o printf.o stdlib.o ctype.o string.o dirent.o time.o
	@list='$(libc_sub_dirs)'; for subdir in $$list; do \
    		echo "make all in $$subdir";\
    		cd $$subdir;\
    		$(MAKE) all CFLAGS="$(CFLAGS) -I $(shell pwd)";\
    		cd ..;\
	done

unistd.o: unistd.c
	$(CC) $(CFLAGS) -c unistd.c -o unistd.o

fcntl.o: fcntl.c
	$(CC) $(CFLAGS) -c fcntl.c -o fcntl.o

malloc.o: malloc.c
	$(CC) $(CFLAGS) -c malloc.c -o malloc.o

errno.o: errno.c
	$(CC) $(CFLAGS) -c errno.c -o errno.o

printf.o: printf.c
	$(CC) $(CFLAGS) -c printf.c -o printf.o

stdlib.o: stdlib.c
	$(CC) $(CFLAGS) -c stdlib.c -o stdlib.o
	
ctype.o: ctype.c
	$(CC) $(CFLAGS) -c ctype.c -o ctype.o

string.o: string.c
	$(CC) $(CFLAGS) -c string.c -o string.o

dirent.o: dirent.c
	$(CC) $(CFLAGS) -c dirent.c -o dirent.o

time.o: time.c
	$(CC) $(CFLAGS) -c time.c -o time.o