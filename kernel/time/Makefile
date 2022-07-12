
all: timer.o sleep.o

CFLAGS += -I .

timer.o: timer.c
	gcc $(CFLAGS) -c timer.c -o timer.o

sleep.o: sleep.c
	gcc $(CFLAGS) -c sleep.c -o sleep.o

clean:
	echo "Done."