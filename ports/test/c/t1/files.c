/* Files and directories: open/read/write/lseek/pread/pwrite/stat/fstat/readdir/
 * rename/unlink/mkdir/rmdir/ftruncate, stdio over a file, under DIR (default /tmp). */
#include <dirent.h>
#include <fcntl.h>
#include <stdlib.h>
#include <sys/stat.h>
#include <unistd.h>
#include "t1.h"

int main(int argc, char **argv) {
    t1_name = "t1-files";
    const char *base = argc > 1 ? argv[1] : "/tmp";
    char dir[256], a[300], b[300], sub[300];
    snprintf(dir, sizeof dir, "%s/t1-files.%d", base, (int)getpid());
    snprintf(a, sizeof a, "%s/a.txt", dir);
    snprintf(b, sizeof b, "%s/b.txt", dir);
    snprintf(sub, sizeof sub, "%s/sub", dir);

    CHECK(mkdir(dir, 0755) == 0);
    CHECK(mkdir(dir, 0755) == -1 && errno == EEXIST);
    CHECK(mkdir(sub, 0700) == 0);

    int fd = open(a, O_CREAT | O_EXCL | O_WRONLY | O_CLOEXEC, 0644);
    CHECK(fd >= 0);
    CHECK(write(fd, "hello, feos\n", 12) == 12);
    CHECK(pwrite(fd, "HELLO", 5, 0) == 5);
    CHECK(lseek(fd, 0, SEEK_CUR) == 12);
    CHECK(close(fd) == 0);
    CHECK(open(a, O_CREAT | O_EXCL | O_WRONLY, 0644) == -1 && errno == EEXIST);

    struct stat st;
    CHECK(stat(a, &st) == 0);
    CHECK(S_ISREG(st.st_mode) && st.st_size == 12);
    CHECK(stat(dir, &st) == 0 && S_ISDIR(st.st_mode));
    CHECK(stat("/no/such/file", &st) == -1 && errno == ENOENT);

    char buf[64] = {0};
    fd = open(a, O_RDONLY);
    CHECK(fd >= 0);
    CHECK(fstat(fd, &st) == 0 && st.st_size == 12);
    CHECK(read(fd, buf, sizeof buf) == 12 && memcmp(buf, "HELLO, feos\n", 12) == 0);
    CHECK(read(fd, buf, sizeof buf) == 0);
    CHECK(lseek(fd, 7, SEEK_SET) == 7);
    CHECK(read(fd, buf, 4) == 4 && memcmp(buf, "feos", 4) == 0);
    CHECK(pread(fd, buf, 5, 0) == 5 && memcmp(buf, "HELLO", 5) == 0);
    CHECK(close(fd) == 0);
    CHECK(close(fd) == -1 && errno == EBADF);

    FILE *f = fopen(a, "a");
    CHECK(f != NULL);
    if (f) {
        fprintf(f, "line %d\n", 2);
        CHECK(fclose(f) == 0);
    }
    f = fopen(a, "r");
    CHECK(f != NULL);
    if (f) {
        char line[64];
        CHECK(fgets(line, sizeof line, f) && strcmp(line, "HELLO, feos\n") == 0);
        CHECK(fgets(line, sizeof line, f) && strcmp(line, "line 2\n") == 0);
        CHECK(fgets(line, sizeof line, f) == NULL);
        fclose(f);
    }

    CHECK(rename(a, b) == 0);
    CHECK(access(a, F_OK) == -1 && errno == ENOENT);
    CHECK(access(b, R_OK) == 0);

    fd = open(b, O_RDWR);
    CHECK(fd >= 0 && ftruncate(fd, 5) == 0 && fstat(fd, &st) == 0 && st.st_size == 5);
    close(fd);

    int found_b = 0, found_sub = 0, found_dot = 0, entries = 0;
    DIR *d = opendir(dir);
    CHECK(d != NULL);
    if (d) {
        struct dirent *e;
        while ((e = readdir(d))) {
            entries++;
            if (strcmp(e->d_name, "b.txt") == 0)
                found_b = e->d_type == DT_REG || e->d_type == DT_UNKNOWN;
            if (strcmp(e->d_name, "sub") == 0)
                found_sub = e->d_type == DT_DIR || e->d_type == DT_UNKNOWN;
            if (strcmp(e->d_name, ".") == 0)
                found_dot = 1;
        }
        closedir(d);
    }
    CHECK(found_b && found_sub && found_dot && entries == 4);

    char cwd[256];
    CHECK(chdir(dir) == 0);
    CHECK(getcwd(cwd, sizeof cwd) && strcmp(cwd, dir) == 0);
    CHECK(access("b.txt", F_OK) == 0);
    CHECK(chdir("/") == 0);

    CHECK(rmdir(dir) == -1 && errno == ENOTEMPTY);
    CHECK(unlink(b) == 0);
    CHECK(unlink(b) == -1 && errno == ENOENT);
    CHECK(rmdir(sub) == 0);
    CHECK(rmdir(dir) == 0);
    CHECK(stat(dir, &st) == -1 && errno == ENOENT);
    return t1_done();
}
