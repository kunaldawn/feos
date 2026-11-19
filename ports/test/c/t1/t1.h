/* Shared by stage T1's C programs: each check prints a line on failure, and a
 * program ends by printing "<name>: ok" (or "<name>: N FAILED") and exiting with
 * the number of failures. */
#include <errno.h>
#include <stdio.h>
#include <string.h>

static int t1_failures;
static const char *t1_name = "t1";

#define CHECK(cond)                                                               \
    do {                                                                          \
        if (!(cond)) {                                                            \
            printf("%s: FAILED %s (line %d, errno %d %s)\n", t1_name, #cond,      \
                   __LINE__, errno, strerror(errno));                             \
            t1_failures++;                                                        \
        }                                                                         \
    } while (0)

static int t1_done(void) {
    if (t1_failures == 0)
        printf("%s: ok\n", t1_name);
    else
        printf("%s: %d FAILED\n", t1_name, t1_failures);
    fflush(stdout);
    return t1_failures;
}
