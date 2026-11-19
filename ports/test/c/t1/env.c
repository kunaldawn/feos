/* The environment: what the shell exported arrives in environ and getenv, and
 * setenv/unsetenv/putenv change this process's copy.
 *   t1-env NAME EXPECTED     checks getenv(NAME) is EXPECTED ("-" for unset) */
#include <stdlib.h>
#include "t1.h"

extern char **environ;

int main(int argc, char **argv) {
    t1_name = "t1-env";
    if (argc == 3) {
        const char *value = getenv(argv[1]);
        printf("t1-env: %s=%s\n", argv[1], value ? value : "(unset)");
        if (strcmp(argv[2], "-") == 0)
            CHECK(value == NULL);
        else
            CHECK(value && strcmp(value, argv[2]) == 0);
        int in_environ = 0;
        size_t len = strlen(argv[1]);
        for (char **e = environ; e && *e; e++)
            if (strncmp(*e, argv[1], len) == 0 && (*e)[len] == '=')
                in_environ++;
        CHECK(in_environ == (value != NULL));
    }
    CHECK(setenv("T1_ENV_SET", "one", 1) == 0);
    CHECK(getenv("T1_ENV_SET") && strcmp(getenv("T1_ENV_SET"), "one") == 0);
    CHECK(setenv("T1_ENV_SET", "two", 0) == 0 && strcmp(getenv("T1_ENV_SET"), "one") == 0);
    CHECK(setenv("T1_ENV_SET", "two", 1) == 0 && strcmp(getenv("T1_ENV_SET"), "two") == 0);
    CHECK(unsetenv("T1_ENV_SET") == 0 && getenv("T1_ENV_SET") == NULL);
    static char put[] = "T1_ENV_PUT=three";
    CHECK(putenv(put) == 0 && strcmp(getenv("T1_ENV_PUT"), "three") == 0);
    return t1_done();
}
