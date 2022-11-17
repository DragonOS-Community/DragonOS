
all: screen_manager.o textui.o textui-render.o

CFLAGS += -I .

screen_manager.o: screen_manager.c
	$(CC) $(CFLAGS) -c screen_manager.c -o screen_manager.o

textui.o: textui.c
	$(CC) $(CFLAGS) -c textui.c -o textui.o

textui-render.o: textui-render.c
	$(CC) $(CFLAGS) -c textui-render.c -o textui-render.o
