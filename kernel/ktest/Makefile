
CFLAGS += -I .


all: bitree.o kfifo.o

bitree.o: test-bitree.c
	gcc $(CFLAGS) -c test-bitree.c -o test-bitree.o

kfifo.o: test-kfifo.c
	gcc $(CFLAGS) -c test-kfifo.c -o test-kfifo.o
