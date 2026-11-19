/* Calls that wait on later kernel stages, each checked for what it should do once
 * that stage is in. The T1 spec marks every mode pending with its stage, so none of
 * these run as passes before then.
 *   t1-pending MODE    one of: isatty raise fork exec getrandom socket wnohang
 *                      spawnmask tcsetpgrp (and blocked, spawnmask's child) */
#include <fcntl.h>
#include <signal.h>
#include <spawn.h>
#include <stdlib.h>
#include <sys/random.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>
#include "t1.h"

extern char **environ;

static volatile sig_atomic_t got;
static void handler(int sig) {
    got = sig;
}

int main(int argc, char **argv) {
    t1_name = "t1-pending";
    const char *mode = argc > 1 ? argv[1] : "";
    if (strcmp(mode, "isatty") == 0) { /* 1c.11 */
        CHECK(isatty(0) == 1);
    } else if (strcmp(mode, "raise") == 0) { /* 1c.9, 1c.10 */
        struct sigaction sa;
        memset(&sa, 0, sizeof sa);
        sa.sa_handler = handler;
        CHECK(sigaction(SIGUSR1, &sa, NULL) == 0);
        CHECK(raise(SIGUSR1) == 0);
        CHECK(got == SIGUSR1);
    } else if (strcmp(mode, "fork") == 0) { /* 1d.1 */
        pid_t pid = fork();
        if (pid == 0)
            _exit(9);
        int status;
        CHECK(pid > 0 && waitpid(pid, &status, 0) == pid && WEXITSTATUS(status) == 9);
    } else if (strcmp(mode, "exec") == 0) { /* 1c.7 */
        char *args[] = {"t1-hello", "exec-ed", NULL};
        execv("/bin/t1-hello", args);
        CHECK(!"execv returned");
    } else if (strcmp(mode, "getrandom") == 0) { /* 1e.1 */
        unsigned char buf[32];
        CHECK(getrandom(buf, sizeof buf, 0) == (ssize_t)sizeof buf);
    } else if (strcmp(mode, "socket") == 0) { /* 1e.3 */
        int fd = socket(AF_INET, SOCK_STREAM, 0);
        CHECK(fd >= 0);
    } else if (strcmp(mode, "wnohang") == 0) { /* 1c.8 */
        int status;
        CHECK(waitpid(-1, &status, WNOHANG) == -1 && errno == ECHILD);
    } else if (strcmp(mode, "spawnmask") == 0) { /* 1c.10: SIGPROCMASK */
        /* POSIX_SPAWN_SETSIGMASK's mask is the child's. */
        posix_spawnattr_t attr;
        sigset_t mask;
        sigemptyset(&mask);
        sigaddset(&mask, SIGUSR1);
        CHECK(posix_spawnattr_init(&attr) == 0);
        CHECK(posix_spawnattr_setsigmask(&attr, &mask) == 0);
        CHECK(posix_spawnattr_setflags(&attr, POSIX_SPAWN_SETSIGMASK) == 0);
        char *args[] = {"t1-pending", "blocked", NULL};
        pid_t pid = -1;
        int status = -1;
        CHECK(posix_spawn(&pid, "/bin/t1-pending", NULL, &attr, args, environ) == 0);
        CHECK(pid > 0 && waitpid(pid, &status, 0) == pid && WIFEXITED(status) &&
              WEXITSTATUS(status) == 0);
        posix_spawnattr_destroy(&attr);
    } else if (strcmp(mode, "blocked") == 0) {
        sigset_t now;
        CHECK(sigprocmask(SIG_BLOCK, NULL, &now) == 0);
        CHECK(sigismember(&now, SIGUSR1) == 1);
    } else if (strcmp(mode, "tcsetpgrp") == 0) { /* 1c.11: terminals */
        /* addtcsetpgrp_np: a child in a group of its own gets the terminal, and the
         * caller takes it back after. /dev/tty, as fds 0-2 may be pipes to the console. */
        int tty = open("/dev/tty", O_RDWR | O_CLOEXEC);
        CHECK(tty >= 0);
        posix_spawnattr_t attr;
        posix_spawn_file_actions_t actions;
        CHECK(posix_spawnattr_init(&attr) == 0);
        CHECK(posix_spawnattr_setflags(&attr, POSIX_SPAWN_SETPGROUP) == 0);
        CHECK(posix_spawn_file_actions_init(&actions) == 0);
        CHECK(posix_spawn_file_actions_addtcsetpgrp_np(&actions, tty) == 0);
        char *args[] = {"t1-hello", "--pgid-own", NULL};
        pid_t pid = -1;
        int status = -1;
        signal(SIGTTOU, SIG_IGN);
        CHECK(posix_spawn(&pid, "/bin/t1-hello", &actions, &attr, args, environ) == 0);
        CHECK(pid > 0 && waitpid(pid, &status, 0) == pid && WIFEXITED(status) &&
              WEXITSTATUS(status) == 20);
        CHECK(tcsetpgrp(tty, getpgrp()) == 0);
        close(tty);
        posix_spawn_file_actions_destroy(&actions);
        posix_spawnattr_destroy(&attr);
    } else {
        printf("t1-pending: unknown mode %s\n", mode);
        return 2;
    }
    return t1_done();
}
