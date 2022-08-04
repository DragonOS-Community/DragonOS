
all: screen_manager.o textui.o textui-render.o

CFLAGS += -I .

screen_manager.o: screen_manager.c
	gcc $(CFLAGS) -c screen_manager.c -o screen_manager.o

textui.o: textui.c
	gcc $(CFLAGS) -c textui.c -o textui.o

textui-render.o: textui-render.c
	gcc $(CFLAGS) -c textui-render.c -o textui-render.o
