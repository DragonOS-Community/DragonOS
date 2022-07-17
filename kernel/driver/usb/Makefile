CFLAGS += -I .

all: usb.o xhci.o

usb.o: usb.c
	gcc $(CFLAGS) -c usb.c -o usb.o

xhci.o: xhci/xhci.c
	gcc $(CFLAGS) -c xhci/xhci.c -o xhci/xhci.o