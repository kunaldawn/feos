/* stdio, arguments and the exit status.
 *   t1-hello [--exit N] [--atexit] [--fd N] [--pgid-own] [ARGS...]
 * prints its arguments, checks formatted output, and exits with N (default 0, the
 * failure count). --atexit registers a handler that prints at exit. For t1-spawn,
 * --fd N exits 10 if descriptor N is open and 11 if not, and --pgid-own exits 20 if
 * this process leads its group and 21 if not. */
#include <fcntl.h>
#include <stdlib.h>
#include <unistd.h>
#include "t1.h"

static void at_exit_handler(void) {
    printf("t1-hello: atexit handler ran\n");
}

int main(int argc, char **argv) {
    t1_name = "t1-hello";
    int code = -1;
    printf("t1-hello: hello from relibc on feos\n");
    printf("t1-hello: argc %d\n", argc);
    for (int i = 0; i < argc; i++) {
        if (strcmp(argv[i], "--exit") == 0 && i + 1 < argc)
            code = atoi(argv[i + 1]);
        if (strcmp(argv[i], "--atexit") == 0)
            atexit(at_exit_handler);
        if (strcmp(argv[i], "--fd") == 0 && i + 1 < argc)
            code = fcntl(atoi(argv[i + 1]), F_GETFD) == -1 ? 11 : 10;
        if (strcmp(argv[i], "--pgid-own") == 0)
            code = getpgid(0) == getpid() ? 20 : 21;
        printf("t1-hello: argv[%d]=%s\n", i, argv[i]);
    }
    char buffer[128];
    snprintf(buffer, sizeof buffer, "%d|%5s|%-3d|%.3f|%x|%lld|%c", 42, "ab", 7, 3.14159, 255,
             -1234567890123LL, 'z');
    CHECK(strcmp(buffer, "42|   ab|7  |3.142|ff|-1234567890123|z") == 0);
    int number = 0;
    CHECK(sscanf("17 words", "%d %63s", &number, buffer) == 2 && number == 17 &&
          strcmp(buffer, "words") == 0);
    fprintf(stderr, "t1-hello: to stderr\n");
    fputs("t1-hello: fputs line\n", stdout);
    CHECK(write(1, "t1-hello: raw write\n", 20) == 20);
    int failures = t1_done();
    if (code >= 0)
        exit(code);
    return failures;
}
