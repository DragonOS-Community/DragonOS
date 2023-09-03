
CFLAGS += -I .


all: ktest.o bitree.o kfifo.o idr.o

ktest.o: ktest.c
	$(CC) $(CFLAGS) -c ktest.c -o ktest.o

bitree.o: test-bitree.c
	$(CC) $(CFLAGS) -c test-bitree.c -o test-bitree.o

kfifo.o: test-kfifo.c
	$(CC) $(CFLAGS) -c test-kfifo.c -o test-kfifo.o

idr.o: test-idr.c
	$(CC) $(CFLAGS) -c test-idr.c -o test-idr.o