
CFLAGS += -I .


all: ktest.o bitree.o kfifo.o

ktest.o: ktest.c
	gcc $(CFLAGS) -c ktest.c -o ktest.o

bitree.o: test-bitree.c
	gcc $(CFLAGS) -c test-bitree.c -o test-bitree.o

kfifo.o: test-kfifo.c
	gcc $(CFLAGS) -c test-kfifo.c -o test-kfifo.o
