/* What the platform reports about itself: uname, sysconf, getauxval, ids, rlimits,
 * getrusage. */
#include <stdlib.h>
#include <sys/auxv.h>
#include <sys/resource.h>
#include <sys/utsname.h>
#include <unistd.h>
#include "t1.h"

/* The AT_ names come from <sys/auxv.h>, feos's own among them. */
#if !defined(AT_PAGESZ) || !defined(AT_HWCAP) || !defined(AT_MINSIGSTKSZ) || !defined(AT_FEOS_EXE)
#error "sys/auxv.h doesn't define the AT_ names"
#endif

int main(void) {
    t1_name = "t1-sys";
    struct utsname u;
    CHECK(uname(&u) == 0);
    CHECK(strcmp(u.sysname, "feos") == 0);
#if defined(__x86_64__)
    CHECK(strcmp(u.machine, "x86_64") == 0);
#elif defined(__aarch64__)
    CHECK(strcmp(u.machine, "aarch64") == 0);
#endif
    printf("t1-sys: uname %s %s %s %s\n", u.sysname, u.nodename, u.release, u.machine);

    long cpus = sysconf(_SC_NPROCESSORS_ONLN);
    long pages = sysconf(_SC_PHYS_PAGES);
    CHECK(cpus >= 1 && cpus <= 64);
    CHECK(pages > 1000);
    CHECK(sysconf(_SC_PAGESIZE) == 4096);
    CHECK(sysconf(_SC_OPEN_MAX) >= 1024);
    printf("t1-sys: cpus %ld pages %ld\n", cpus, pages);

    CHECK(getauxval(AT_PAGESZ) == 4096);
    CHECK(getauxval(AT_PHDR) != 0 && getauxval(AT_PHNUM) > 0);
    CHECK(getauxval(AT_RANDOM) != 0);
    CHECK(AT_HWCAP == 16 && AT_FEOS_FEATURES == 0x66650003);
    CHECK(getauxval(AT_FEOS_ABI) >= 2);
    const char *exe = (const char *)getauxval(AT_FEOS_EXE);
    CHECK(exe && strstr(exe, "t1-sys") != NULL);
    printf("t1-sys: exe %s\n", exe ? exe : "(none)");

    CHECK(getpid() > 1);
    CHECK(getppid() >= 1);
    CHECK(getuid() == geteuid() && getgid() == getegid());
    CHECK(getpgid(0) > 0 && getsid(0) > 0);

    struct rlimit rl;
    CHECK(getrlimit(RLIMIT_NOFILE, &rl) == 0 && rl.rlim_cur >= 1024);
    CHECK(getrlimit(RLIMIT_STACK, &rl) == 0 && rl.rlim_cur == 8 * 1024 * 1024);
    struct rusage ru;
    CHECK(getrusage(RUSAGE_SELF, &ru) == 0);

    char cwd[256];
    CHECK(getcwd(cwd, sizeof cwd) != NULL && cwd[0] == '/');
    char link[256];
    ssize_t n = readlink("/proc/self/exe", link, sizeof link - 1);
    CHECK(n > 0);
    if (n > 0) {
        link[n] = 0;
        CHECK(exe && strcmp(link, exe) == 0);
    }
    return t1_done();
}
