/* The calls of later kernel stages are made at their numbers and give back the
 * kernel's answer: ENOSYS until their stage lands (never a made-up success). A call
 * whose stage's feature bit the kernel reports (AT_FEOS_FEATURES) is skipped here:
 * its own test covers it then. */
#include <signal.h>
#include <stdlib.h>
#include <sys/auxv.h>
#include <sys/random.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <termios.h>
#include <unistd.h>
#include "t1.h"

static unsigned long features;

#define ENOSYS_CHECK(stage, bit, expr)                                             \
    do {                                                                           \
        if (features & (1UL << (bit))) {                                           \
            printf("t1-enosys: %s landed, %s not checked\n", stage, #expr);        \
            break;                                                                 \
        }                                                                          \
        errno = 0;                                                                 \
        long r = (long)(expr);                                                     \
        printf("t1-enosys: %s %s -> %ld errno %d\n", stage, #expr, r, errno);      \
        CHECK(r == -1 && errno == ENOSYS);                                         \
    } while (0)

int main(void) {
    t1_name = "t1-enosys";
    features = getauxval(AT_FEOS_FEATURES);
    printf("t1-enosys: kernel features %#lx\n", features);
    CHECK(features & (1UL << 16)); /* 1c.6, which relibc needs */
    unsigned char buf[8];
    struct termios t;
    struct sigaction sa;
    sigset_t set;
    ENOSYS_CHECK("1c.8", 18, kill(getpid(), 0));
    ENOSYS_CHECK("1c.8", 18, waitpid(-1, NULL, WNOHANG));
    ENOSYS_CHECK("1c.9", 19, sigaction(SIGUSR1, NULL, &sa));
    ENOSYS_CHECK("1c.10", 20, sigprocmask(SIG_BLOCK, NULL, &set));
    ENOSYS_CHECK("1c.11", 21, tcgetattr(0, &t));
    ENOSYS_CHECK("1d.1", 23, fork());
    ENOSYS_CHECK("1e.1", 24, getrandom(buf, sizeof buf, 0));
    ENOSYS_CHECK("1e.3", 26, socket(AF_INET, SOCK_STREAM, 0));
    return t1_done();
}
