
all: timer.o sleep.o

CFLAGS += -I .

timer.o: timer.c
	$(CC) $(CFLAGS) -c timer.c -o timer.o

sleep.o: sleep.c
	$(CC) $(CFLAGS) -c sleep.c -o sleep.o

clean:
	echo "Done."