/* t5-c-hello: C built by `clang --target=<arch>-unknown-feos` with no other flags. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int main(int argc, char **argv) {
#if !defined(__feos__) || !defined(__unix__) || !defined(__ELF__) || defined(__linux__)
    puts("FAILED: predefined macros");
    return 1;
#endif
    printf("hello from C on feos, %d argument(s)\n", argc - 1);
    for (int i = 1; i < argc; i++)
        printf("argv[%d]=%s\n", i, argv[i]);
    char *copy = strdup(argc > 1 ? argv[1] : "none");
    printf("strdup: %s\n", copy);
    free(copy);
#ifdef __x86_64__
    puts("arch: x86_64");
#elif defined(__aarch64__)
    puts("arch: aarch64");
#endif
    puts("t5-c-hello: ok");
    return 0;
}
