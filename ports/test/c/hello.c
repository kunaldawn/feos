/* A first relibc program: stdio, the heap, strings, math, the environment,
 * files and a thread, each checked. Prints "relibc-hello: ok" when all pass,
 * and exits with the number of failures. */
#include <errno.h>
#include <fcntl.h>
#include <math.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static int failures;

#define CHECK(cond) do { if (!(cond)) { printf("relibc-hello: FAILED %s (line %d)\n", #cond, __LINE__); failures++; } } while (0)

static int compare(const void *a, const void *b) {
    return *(const int *)a - *(const int *)b;
}

static void *thread_main(void *arg) {
    *(int *)arg += 41;
    return arg;
}

int main(int argc, char **argv) {
    char buffer[128];
    printf("relibc-hello: argc %d argv[0] %s\n", argc, argv[0]);

    snprintf(buffer, sizeof buffer, "%d %s %.3f %x", 42, "words", 3.14159, 255);
    CHECK(strcmp(buffer, "42 words 3.142 ff") == 0);

    char *heap = malloc(1 << 20);
    CHECK(heap != NULL);
    memset(heap, 'x', 1 << 20);
    heap = realloc(heap, 2 << 20);
    CHECK(heap != NULL && heap[(1 << 20) - 1] == 'x');
    free(heap);

    int numbers[] = {5, 3, 9, 1, 7};
    qsort(numbers, 5, sizeof numbers[0], compare);
    CHECK(numbers[0] == 1 && numbers[4] == 9);

    CHECK(fabs(sqrt(2.0) * sqrt(2.0) - 2.0) < 1e-12);
    CHECK(strtod("2.5e3", NULL) == 2500.0);

    CHECK(setenv("RELIBC_HELLO", "set", 1) == 0);
    CHECK(getenv("RELIBC_HELLO") && strcmp(getenv("RELIBC_HELLO"), "set") == 0);

    const char *path = argc > 1 ? argv[1] : "relibc-hello.tmp";
    FILE *file = fopen(path, "w");
    CHECK(file != NULL);
    if (file) {
        fprintf(file, "line %d\n", 7);
        fclose(file);
        int fd = open(path, O_RDONLY);
        CHECK(fd >= 0);
        ssize_t got = read(fd, buffer, sizeof buffer - 1);
        CHECK(got == 7 && memcmp(buffer, "line 7\n", 7) == 0);
        close(fd);
        CHECK(unlink(path) == 0);
        CHECK(open(path, O_RDONLY) < 0 && errno == ENOENT);
    }

    /* qemu-user only clones a thread with CLONE_SYSVSEM, which relibc's Linux
     * pthread_create leaves out: relibc-linux.sh sets this under qemu-user. */
    if (!getenv("RELIBC_HELLO_NO_THREADS")) {
        int value = 1;
        pthread_t thread;
        void *result = NULL;
        int created = pthread_create(&thread, NULL, thread_main, &value);
        CHECK(created == 0);
        if (created == 0) {
            CHECK(pthread_join(thread, &result) == 0);
            CHECK(result == &value && value == 42);
        }
    } else {
        printf("relibc-hello: threads not checked\n");
    }

    if (failures == 0)
        printf("relibc-hello: ok\n");
    return failures;
}
