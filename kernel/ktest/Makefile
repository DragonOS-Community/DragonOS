
CFLAGS += -I .


all: ktest.o bitree.o kfifo.o mutex.o idr.o

ktest.o: ktest.c
	$(CC) $(CFLAGS) -c ktest.c -o ktest.o

bitree.o: test-bitree.c
	$(CC) $(CFLAGS) -c test-bitree.c -o test-bitree.o

kfifo.o: test-kfifo.c
	$(CC) $(CFLAGS) -c test-kfifo.c -o test-kfifo.o

mutex.o: test-mutex.c
	$(CC) $(CFLAGS) -c test-mutex.c -o test-mutex.o

idr.o: test-idr.c
	$(CC) $(CFLAGS) -c test-idr.c -o test-idr.o