CFLAGS += -I .

all: usb.o xhci.o

usb.o: usb.c
	$(CC) $(CFLAGS) -c usb.c -o usb.o

xhci.o: xhci/xhci.c
	$(CC) $(CFLAGS) -c xhci/xhci.c -o xhci/xhci.o