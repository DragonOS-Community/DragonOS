#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include <fcntl.h>
#include <termios.h>
#include <pty.h>

int main()
{
	int ptm, pts;
	char name[256];
	struct termios term;

	if (openpty(&ptm, &pts, name, NULL, NULL) == -1) {
		perror("openpty");
		exit(EXIT_FAILURE);
	}

	printf("slave name: %s fd: %d\n", name,pts);

	tcgetattr(pts, &term);
	term.c_lflag &= ~(ICANON | ECHO);
	term.c_cc[VMIN] = 1;
	term.c_cc[VTIME] = 0;
	tcsetattr(pts, TCSANOW, &term);
	
	printf("before print to pty slave\n");

	dprintf(pts, "Hello world!\n");

	char buf[256];
	ssize_t n = read(ptm, buf, sizeof(buf));
	if (n > 0) {
		printf("read %ld bytes from slave: %.*s", n, (int)n, buf);
	}

	dprintf(ptm, "hello world from master\n");

	char nbuf[256];
	ssize_t nn = read(pts, nbuf, sizeof(nbuf));
	if (nn > 0) {
		printf("read %ld bytes from master: %.*s", nn, (int)nn, nbuf);
	}

	close(ptm);
	close(pts);

	return 0;
}