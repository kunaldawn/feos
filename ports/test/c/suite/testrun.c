/* testrun: the libc suite's runner on feos (ports/test/libc-suite.sh).
 *
 *   testrun [-C DIR] [-t SECONDS] [-w WRAPPER] [-v] LIST
 *   testrun --sleep SECONDS        (sleeps, then exits 0: testrun's own test uses it)
 *
 * Runs every test LIST names from DIR (default: LIST's directory), each with
 * posix_spawn, stdin from /dev/null and stdout and stderr into files, and checks it.
 * One line of LIST per test, fields separated by spaces:
 *
 *   expect NAME BIN STDOUT STDERR [ARG...]   exit 0, and stdout and stderr exactly as the
 *                                            files STDOUT and STDERR ("-" is empty)
 *   status NAME BIN [ARG...]                 exit 0 (libc-test's convention: a failing
 *                                            test prints why and exits non-zero)
 *   pending NAME STAGE WHY...                not run: waits on kernel stage STAGE
 *   skip NAME WHY...                         not run, for the reason given
 *
 * -w runs each test as WRAPPER BIN ARG... (qemu-user, for the Linux reference of another
 * architecture). Blank lines and lines starting with '#' are ignored. Each test prints one line, PASS,
 * FAIL, PEND or SKIP and its name, and a failure its output after it; the last line is
 * "testrun: LIST: P passed, F failed, N pending, S skipped". Exits 0 when nothing failed.
 *
 * feos has no kill(2) until kernel stage 1c.8, so a test still running after the timeout
 * (-t, default 30 s) can't be stopped: it fails as TIMEOUT and is left running, and the
 * next test starts. A test that hangs belongs in LIST as pending. Each test's output goes
 * to files of its own in /tmp (testrun-N.stdout, .stderr), removed once checked unless
 * the test was left running.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

extern char **environ;

#define MAX_ARGS 32
#define SHOW_BYTES 1200

static int verbose;
static int passed, failed, pending, skipped, timeouts;
static const char *list_name;

static double now(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

static void summary(void) {
    printf("testrun: %s: %d passed, %d failed, %d pending, %d skipped\n", list_name, passed,
           failed, pending, skipped);
    fflush(stdout);
}

/* One test's child, waited for by a thread of its own, so that a test which never ends
 * can be left behind (feos has no kill(2) before stage 1c.8). */
struct waiter {
    pthread_mutex_t lock;
    pthread_cond_t done_cv;
    pid_t pid;
    int done, status, error;
};

static void *wait_child(void *arg) {
    struct waiter *w = arg;
    int status = 0, error = 0;
    while (waitpid(w->pid, &status, 0) < 0) {
        if (errno != EINTR) {
            error = errno;
            break;
        }
    }
    pthread_mutex_lock(&w->lock);
    w->status = status;
    w->error = error;
    w->done = 1;
    pthread_cond_signal(&w->done_cv);
    pthread_mutex_unlock(&w->lock);
    return NULL;
}

/* The whole of a file, NUL-terminated; *len its length. A missing file is empty. */
static char *slurp(const char *path, size_t *len) {
    *len = 0;
    char *data = malloc(1);
    if (!data)
        return NULL;
    data[0] = 0;
    if (!path || strcmp(path, "-") == 0)
        return data;
    FILE *f = fopen(path, "rb");
    if (!f)
        return data;
    size_t cap = 1;
    char buf[4096];
    size_t n;
    while ((n = fread(buf, 1, sizeof buf, f)) > 0) {
        if (*len + n + 1 > cap) {
            cap = (*len + n + 1) * 2;
            char *grown = realloc(data, cap);
            if (!grown) {
                free(data);
                fclose(f);
                return NULL;
            }
            data = grown;
        }
        memcpy(data + *len, buf, n);
        *len += n;
    }
    data[*len] = 0;
    fclose(f);
    return data;
}

static void show(const char *label, const char *data, size_t len) {
    printf("  --- %s (%zu bytes)%s\n", label, len, len > SHOW_BYTES ? ", start" : "");
    fwrite(data, 1, len > SHOW_BYTES ? SHOW_BYTES : len, stdout);
    if (len && data[(len > SHOW_BYTES ? SHOW_BYTES : len) - 1] != '\n')
        putchar('\n');
}

/* Runs one test, its output in OUT and ERR; returns its wait status, -1 with errno when
 * it didn't start, or -2 when it was still running after TIMEOUT seconds (it's left
 * running, with its waiter thread). */
static int run(char *const argv[], const char *out, const char *err, double timeout) {
    posix_spawn_file_actions_t fa;
    posix_spawn_file_actions_init(&fa);
    posix_spawn_file_actions_addopen(&fa, 0, "/dev/null", O_RDONLY, 0);
    posix_spawn_file_actions_addopen(&fa, 1, out, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    posix_spawn_file_actions_addopen(&fa, 2, err, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    pid_t pid;
    int e = posix_spawn(&pid, argv[0], &fa, NULL, argv, environ);
    posix_spawn_file_actions_destroy(&fa);
    if (e) {
        errno = e;
        return -1;
    }
    struct waiter *w = calloc(1, sizeof *w);
    pthread_t thread;
    if (!w || pthread_mutex_init(&w->lock, NULL) || pthread_cond_init(&w->done_cv, NULL)) {
        errno = ENOMEM;
        return -1;
    }
    w->pid = pid;
    if (pthread_create(&thread, NULL, wait_child, w) != 0) {
        errno = EAGAIN;
        return -1;
    }
    struct timespec deadline;
    clock_gettime(CLOCK_REALTIME, &deadline);
    deadline.tv_sec += (time_t)timeout;
    pthread_mutex_lock(&w->lock);
    while (!w->done) {
        if (pthread_cond_timedwait(&w->done_cv, &w->lock, &deadline) == ETIMEDOUT && !w->done) {
            pthread_mutex_unlock(&w->lock);
            /* Left behind: the waiter keeps w. */
            pthread_detach(thread);
            return -2;
        }
    }
    pthread_mutex_unlock(&w->lock);
    pthread_join(thread, NULL);
    int status = w->status, error = w->error;
    free(w);
    if (error) {
        errno = error;
        return -1;
    }
    return status;
}

static int split(char *line, char **fields, int max) {
    int n = 0;
    char *save = NULL;
    for (char *tok = strtok_r(line, " \t\n", &save); tok && n < max; tok = strtok_r(NULL, " \t\n", &save))
        fields[n++] = tok;
    return n;
}

int main(int argc, char **argv) {
    const char *dir = NULL;
    double timeout = 30;
    /* For testrun's own test (ports/test/suite-selftest): a test that outlives -t. */
    if (argc == 3 && strcmp(argv[1], "--sleep") == 0) {
        sleep((unsigned)atoi(argv[2]));
        return 0;
    }
    int opt;
    const char *wrapper = NULL;
    while ((opt = getopt(argc, argv, "C:t:w:v")) != -1) {
        switch (opt) {
        case 'C': dir = optarg; break;
        case 'w': wrapper = optarg; break;
        case 't': timeout = atof(optarg); break;
        case 'v': verbose = 1; break;
        default:
            fprintf(stderr, "usage: testrun [-C DIR] [-t SECONDS] [-w WRAPPER] [-v] LIST\n");
            return 64;
        }
    }
    if (optind + 1 != argc) {
        fprintf(stderr, "usage: testrun [-C DIR] [-t SECONDS] [-w WRAPPER] [-v] LIST\n");
        return 64;
    }
    list_name = argv[optind];
    char list_path[1024];
    if (list_name[0] == '/' || !dir) {
        snprintf(list_path, sizeof list_path, "%s", list_name);
    } else {
        snprintf(list_path, sizeof list_path, "%s/%s", dir, list_name);
    }
    FILE *list = fopen(list_path, "r");
    if (!list) {
        printf("testrun: %s: %s\n", list_path, strerror(errno));
        return 64;
    }
    char where[1024];
    if (!dir) {
        snprintf(where, sizeof where, "%s", list_path);
        char *slash = strrchr(where, '/');
        if (slash)
            *slash = 0;
        else
            strcpy(where, ".");
        dir = where;
    }
    if (chdir(dir) != 0) {
        printf("testrun: chdir %s: %s\n", dir, strerror(errno));
        return 64;
    }
    setvbuf(stdout, NULL, _IOLBF, 0);

    char line[4096];
    while (fgets(line, sizeof line, list)) {
        char *f[MAX_ARGS + 8];
        if (line[0] == '#')
            continue;
        int n = split(line, f, MAX_ARGS + 6);
        if (n == 0)
            continue;
        if (strcmp(f[0], "pending") == 0 && n >= 3) {
            printf("PEND %s (stage %s)\n", f[1], f[2]);
            pending++;
            continue;
        }
        if (strcmp(f[0], "skip") == 0 && n >= 2) {
            printf("SKIP %s\n", f[1]);
            skipped++;
            continue;
        }
        int expect = strcmp(f[0], "expect") == 0;
        if (!(expect && n >= 5) && !(strcmp(f[0], "status") == 0 && n >= 3)) {
            printf("FAIL bad list line: %s\n", f[0]);
            failed++;
            continue;
        }
        const char *name = f[1];
        char *targv[MAX_ARGS + 3];
        int first = expect ? 5 : 3, k = 0;
        if (wrapper)
            targv[k++] = (char *)wrapper;
        targv[k++] = f[2];
        for (int i = first; i < n && k < MAX_ARGS; i++)
            targv[k++] = f[i];
        targv[k] = NULL;

        char out_path[64], err_path[64];
        static int serial;
        serial++;
        snprintf(out_path, sizeof out_path, "/tmp/testrun-%d.stdout", serial);
        snprintf(err_path, sizeof err_path, "/tmp/testrun-%d.stderr", serial);
        double started = now();
        int status = run(targv, out_path, err_path, timeout);
        int spawn_errno = errno;
        double took = now() - started;
        if (status == -2) {
            failed++;
            timeouts++;
            printf("TIMEOUT %s: still running after %.0f s, left running\n", name, timeout);
            continue;
        }

        size_t olen, elen;
        char *out = slurp(out_path, &olen);
        char *err = slurp(err_path, &elen);
        char why[256] = "";
        int ok = 1;
        if (status < 0) {
            snprintf(why, sizeof why, "didn't start: %s", strerror(spawn_errno));
            ok = 0;
        } else if (WIFSIGNALED(status)) {
            snprintf(why, sizeof why, "killed by signal %d", WTERMSIG(status));
            ok = 0;
        } else if (WEXITSTATUS(status) != 0) {
            snprintf(why, sizeof why, "exit status %d", WEXITSTATUS(status));
            ok = 0;
        }
        char *want_out = NULL, *want_err = NULL;
        size_t wolen = 0, welen = 0;
        int out_bad = 0, err_bad = 0;
        if (expect && status >= 0) {
            want_out = slurp(f[3], &wolen);
            want_err = slurp(f[4], &welen);
            if (!want_out || !want_err) {
                snprintf(why + strlen(why), sizeof why - strlen(why), "%sno memory", *why ? ", " : "");
                ok = 0;
            } else {
                out_bad = wolen != olen || memcmp(want_out, out, olen) != 0;
                err_bad = welen != elen || memcmp(want_err, err, elen) != 0;
                if (out_bad || err_bad) {
                    const char *what = out_bad && err_bad ? "stdout and stderr differ"
                                       : out_bad          ? "stdout differs"
                                                          : "stderr differs";
                    snprintf(why + strlen(why), sizeof why - strlen(why), "%s%s", *why ? ", " : "", what);
                    ok = 0;
                }
            }
        }
        if (ok) {
            passed++;
            printf("PASS %s (%.2fs)\n", name, took);
            if (verbose) {
                show("stdout", out, olen);
                show("stderr", err, elen);
            }
        } else {
            failed++;
            printf("FAIL %s: %s\n", name, why);
            if (out_bad)
                show("expected stdout", want_out, wolen);
            show("stdout", out, olen);
            if (err_bad)
                show("expected stderr", want_err, welen);
            show("stderr", err, elen);
        }
        free(out);
        free(err);
        free(want_out);
        free(want_err);
        unlink(out_path);
        unlink(err_path);
    }
    fclose(list);
    summary();
    if (timeouts)
        printf("testrun: %d tests left running\n", timeouts);
    return failed ? 1 : 0;
}
