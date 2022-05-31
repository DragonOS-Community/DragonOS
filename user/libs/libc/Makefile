all: libc

CFLAGS += -I .

libc_sub_dirs=math sys


libc: unistd.o fcntl.o malloc.o errno.o printf.o stdlib.o ctype.o string.o dirent.o
	@list='$(libc_sub_dirs)'; for subdir in $$list; do \
    		echo "make all in $$subdir";\
    		cd $$subdir;\
    		$(MAKE) all CFLAGS="$(CFLAGS) -I $(shell pwd)";\
    		cd ..;\
	done

unistd.o: unistd.c
	gcc $(CFLAGS) -c unistd.c -o unistd.o

fcntl.o: fcntl.c
	gcc $(CFLAGS) -c fcntl.c -o fcntl.o

malloc.o: malloc.c
	gcc $(CFLAGS) -c malloc.c -o malloc.o

errno.o: errno.c
	gcc $(CFLAGS) -c errno.c -o errno.o

printf.o: printf.c
	gcc $(CFLAGS) -c printf.c -o printf.o

stdlib.o: stdlib.c
	gcc $(CFLAGS) -c stdlib.c -o stdlib.o
	
ctype.o: ctype.c
	gcc $(CFLAGS) -c ctype.c -o ctype.o

string.o: string.c
	gcc $(CFLAGS) -c string.c -o string.o

dirent.o: dirent.c
	gcc $(CFLAGS) -c dirent.c -o dirent.o