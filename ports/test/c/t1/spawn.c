/* posix_spawn of other C programs, with arguments, an environment, file actions and
 * attributes, posix_spawnp's PATH search, and waitpid for their exit status.
 *   t1-spawn DIR     DIR holds t1-hello and t1-env (default /bin) */
#include <fcntl.h>
#include <signal.h>
#include <spawn.h>
#include <stdlib.h>
#include <sys/auxv.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>
#include "t1.h"

extern char **environ;

static int run_attr(const char *path, char *const argv[], char *const envp[],
                    const posix_spawn_file_actions_t *actions,
                    const posix_spawnattr_t *attr, int *status) {
    pid_t pid = -1;
    int err = posix_spawn(&pid, path, actions, attr, argv, envp);
    if (err != 0) {
        printf("t1-spawn: posix_spawn %s: %s\n", path, strerror(err));
        return -1;
    }
    CHECK(pid > 0);
    pid_t got = waitpid(pid, status, 0);
    CHECK(got == pid);
    return got == pid ? 0 : -1;
}

static int run(const char *path, char *const argv[], char *const envp[],
               const posix_spawn_file_actions_t *actions, int *status) {
    return run_attr(path, argv, envp, actions, NULL, status);
}

/* The exit status of t1-hello run with ARGV, or -1. */
static int exit_of(const char *path, char *const argv[],
                   const posix_spawn_file_actions_t *actions, const posix_spawnattr_t *attr) {
    int status = -1;
    if (run_attr(path, argv, environ, actions, attr, &status) != 0 || !WIFEXITED(status))
        return -1;
    return WEXITSTATUS(status);
}

int main(int argc, char **argv) {
    t1_name = "t1-spawn";
    const char *dir = argc > 1 ? argv[1] : "/bin";
    char hello[256], env[256];
    snprintf(hello, sizeof hello, "%s/t1-hello", dir);
    snprintf(env, sizeof env, "%s/t1-env", dir);
    int status = -1;

    /* Exit status through waitpid. */
    char *hello_args[] = {"t1-hello", "--exit", "42", "from-spawn", NULL};
    CHECK(run(hello, hello_args, environ, NULL, &status) == 0);
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 42);
    printf("t1-spawn: child exited %d\n", WEXITSTATUS(status));

    char *ok_args[] = {"t1-hello", NULL};
    CHECK(run(hello, ok_args, environ, NULL, &status) == 0);
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0);

    /* An environment of our choosing. */
    char *env_args[] = {"t1-env", "T1_CHILD", "from-parent", NULL};
    char *env_vars[] = {"T1_CHILD=from-parent", "OTHER=x", NULL};
    CHECK(run(env, env_args, env_vars, NULL, &status) == 0);
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0);
    char *unset_args[] = {"t1-env", "T1_CHILD", "-", NULL};
    char *no_vars[] = {NULL};
    CHECK(run(env, unset_args, no_vars, NULL, &status) == 0);
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0);

    /* The child's stdout into a pipe, through file actions. */
    int fds[2];
    CHECK(pipe(fds) == 0);
    posix_spawn_file_actions_t actions;
    CHECK(posix_spawn_file_actions_init(&actions) == 0);
    CHECK(posix_spawn_file_actions_adddup2(&actions, fds[1], 1) == 0);
    CHECK(posix_spawn_file_actions_addclose(&actions, fds[0]) == 0);
    CHECK(posix_spawn_file_actions_addclose(&actions, fds[1]) == 0);
    char *piped_args[] = {"t1-hello", "through-a-pipe", NULL};
    pid_t pid = -1;
    CHECK(posix_spawn(&pid, hello, &actions, NULL, piped_args, environ) == 0);
    close(fds[1]);
    char out[2048];
    size_t have = 0;
    ssize_t n;
    while (have < sizeof out - 1 && (n = read(fds[0], out + have, sizeof out - 1 - have)) > 0)
        have += (size_t)n;
    out[have] = 0;
    close(fds[0]);
    CHECK(waitpid(pid, &status, 0) == pid && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    CHECK(strstr(out, "t1-hello: argv[1]=through-a-pipe") != NULL);
    CHECK(strstr(out, "t1-hello: ok") != NULL);
    posix_spawn_file_actions_destroy(&actions);

    /* An open action: the child's stdout to a file. */
    char path[64];
    snprintf(path, sizeof path, "/tmp/t1-spawn.%d", (int)getpid());
    CHECK(posix_spawn_file_actions_init(&actions) == 0);
    /* The action keeps a copy of the path: the caller's buffer may change after. */
    char scratch[64];
    snprintf(scratch, sizeof scratch, "%s", path);
    CHECK(posix_spawn_file_actions_addopen(&actions, 1, scratch, O_WRONLY | O_CREAT | O_TRUNC, 0644) == 0);
    snprintf(scratch, sizeof scratch, "/no/such/dir/x");
    char *file_args[] = {"t1-hello", "into-a-file", NULL};
    CHECK(run(hello, file_args, environ, &actions, &status) == 0);
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0);
    posix_spawn_file_actions_destroy(&actions);
    FILE *f = fopen(path, "r");
    CHECK(f != NULL);
    if (f) {
        have = fread(out, 1, sizeof out - 1, f);
        out[have] = 0;
        fclose(f);
        CHECK(strstr(out, "argv[1]=into-a-file") != NULL);
    }
    unlink(path);

    /* posix_spawnp searches PATH. */
    CHECK(setenv("PATH", dir, 1) == 0);
    pid = -1;
    CHECK(posix_spawnp(&pid, "t1-hello", NULL, NULL, hello_args, environ) == 0);
    CHECK(pid > 0 && waitpid(pid, &status, 0) == pid && WEXITSTATUS(status) == 42);

    /* A directory where the program should be is EACCES, which doesn't end the search
     * but is the error when nothing later in PATH has it. */
    char denied[64], search[320];
    snprintf(denied, sizeof denied, "/tmp/t1-spawnp.%d", (int)getpid());
    snprintf(path, sizeof path, "%s/t1-hello", denied);
    CHECK(mkdir(denied, 0755) == 0 && mkdir(path, 0755) == 0);
    snprintf(search, sizeof search, "/tmp/t1-no-such-dir:%s:%s", denied, dir);
    CHECK(setenv("PATH", search, 1) == 0);
    pid = -1;
    CHECK(posix_spawnp(&pid, "t1-hello", NULL, NULL, hello_args, environ) == 0);
    CHECK(pid > 0 && waitpid(pid, &status, 0) == pid && WEXITSTATUS(status) == 42);
    snprintf(search, sizeof search, "/tmp/t1-no-such-dir:%s", denied);
    CHECK(setenv("PATH", search, 1) == 0);
    CHECK(posix_spawnp(&pid, "t1-hello", NULL, NULL, hello_args, environ) == EACCES);
    CHECK(setenv("PATH", "/tmp/t1-no-such-dir", 1) == 0);
    CHECK(posix_spawnp(&pid, "t1-hello", NULL, NULL, hello_args, environ) == ENOENT);
    rmdir(path);
    rmdir(denied);
    /* With PATH unset the search is /bin:/usr/bin. */
    if (strcmp(dir, "/bin") == 0) {
        CHECK(unsetenv("PATH") == 0);
        pid = -1;
        CHECK(posix_spawnp(&pid, "t1-hello", NULL, NULL, hello_args, environ) == 0);
        CHECK(pid > 0 && waitpid(pid, &status, 0) == pid && WEXITSTATUS(status) == 42);
    }

    /* closefrom_np: the child closes every descriptor from the one given up. */
    int held = open(hello, O_RDONLY);
    CHECK(held >= 0 && dup2(held, 7) == 7);
    char *fd_args[] = {"t1-hello", "--fd", "7", NULL};
    CHECK(exit_of(hello, fd_args, NULL, NULL) == 10);
    CHECK(posix_spawn_file_actions_init(&actions) == 0);
    CHECK(posix_spawn_file_actions_addclosefrom_np(&actions, 3) == 0);
    CHECK(exit_of(hello, fd_args, &actions, NULL) == 11);
    posix_spawn_file_actions_destroy(&actions);
    CHECK(posix_spawn_file_actions_init(&actions) == 0);
    CHECK(posix_spawn_file_actions_addclosefrom_np(&actions, 8) == 0);
    CHECK(exit_of(hello, fd_args, &actions, NULL) == 10);
    CHECK(posix_spawn_file_actions_addclosefrom_np(&actions, -1) == EBADF);
    posix_spawn_file_actions_destroy(&actions);
    close(7);
    close(held);

    /* tcsetpgrp_np reaches SPAWN2, which refuses it until there are terminals (stage
     * 1c.11, feature bit 21; t1-pending's tcsetpgrp mode checks it from then). */
    CHECK(posix_spawn_file_actions_init(&actions) == 0);
    CHECK(posix_spawn_file_actions_addtcsetpgrp_np(&actions, 0) == 0);
    CHECK(posix_spawn_file_actions_addtcsetpgrp_np(&actions, -1) == EBADF);
    if (!(getauxval(AT_FEOS_FEATURES) & (1UL << 21)))
        CHECK(posix_spawn(&pid, hello, &actions, NULL, ok_args, environ) == EINVAL);
    posix_spawn_file_actions_destroy(&actions);

    /* Attributes whose flag isn't set are ignored, as POSIX says. */
    posix_spawnattr_t attr;
    sigset_t mask;
    sigemptyset(&mask);
    sigaddset(&mask, SIGUSR1);
    char *pgid_args[] = {"t1-hello", "--pgid-own", NULL};
    pid_t group = getpgid(0);
    CHECK(group > 0);
    CHECK(posix_spawnattr_init(&attr) == 0);
    CHECK(posix_spawnattr_setsigmask(&attr, &mask) == 0);
    CHECK(posix_spawnattr_setsigdefault(&attr, &mask) == 0);
    CHECK(posix_spawnattr_setpgroup(&attr, group) == 0);
    CHECK(exit_of(hello, pgid_args, NULL, &attr) == 21);
    /* With their flags: a group of its own for pgroup 0, the caller's when named. */
    CHECK(posix_spawnattr_setflags(&attr, POSIX_SPAWN_SETPGROUP | POSIX_SPAWN_SETSIGMASK |
                                              POSIX_SPAWN_SETSIGDEF) == 0);
    CHECK(exit_of(hello, pgid_args, NULL, &attr) == 21);
    CHECK(posix_spawnattr_setpgroup(&attr, 0) == 0);
    CHECK(exit_of(hello, pgid_args, NULL, &attr) == 20);
    posix_spawnattr_destroy(&attr);
    printf("t1-spawn: attributes ok\n");

    /* A program that isn't there is the call's error, not a child's status. */
    char *missing_args[] = {"missing", NULL};
    CHECK(posix_spawn(&pid, "/no/such/program", NULL, NULL, missing_args, environ) == ENOENT);

    return t1_done();
}
