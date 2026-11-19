/* sysroot-check: checks the sysroot installed in the feos image (ports/sysroot.sh)
 * against its manifest, /usr/share/feos-sysroot/MANIFEST: every file there, with its
 * size and FNV-1a hash, and the archives' and objects' magic numbers.
 *
 *   sysroot-check [ROOT]       # default /
 *
 * Prints "sysroot-check: N files ok" and exits 0, or each problem and exits 1.
 */
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

static int bad;

static void problem(const char *path, const char *what) {
    printf("sysroot-check: %s: %s\n", path, what);
    bad++;
}

int main(int argc, char **argv) {
    const char *root = argc > 1 ? argv[1] : "";
    char path[1024];
    snprintf(path, sizeof path, "%s/usr/share/feos-sysroot/MANIFEST", root);
    FILE *manifest = fopen(path, "r");
    if (!manifest) {
        printf("sysroot-check: %s: %s\n", path, strerror(errno));
        return 1;
    }
    unsigned long long size;
    char hash[32], rel[900];
    int files = 0, archives = 0, objects = 0, headers = 0;
    while (fscanf(manifest, "%llu %31s %899s", &size, hash, rel) == 3) {
        files++;
        snprintf(path, sizeof path, "%s%s", root, rel);
        FILE *f = fopen(path, "rb");
        if (!f) {
            problem(rel, strerror(errno));
            continue;
        }
        uint64_t h = 0xcbf29ce484222325ull;
        unsigned long long got = 0;
        unsigned char buf[8192], head[8] = {0};
        size_t n;
        while ((n = fread(buf, 1, sizeof buf, f)) > 0) {
            if (got < 8)
                memcpy(head + got, buf, n < 8 - got ? n : 8 - got);
            for (size_t i = 0; i < n; i++)
                h = (h ^ buf[i]) * 0x100000001b3ull;
            got += n;
        }
        fclose(f);
        char hex[17];
        snprintf(hex, sizeof hex, "%016llx", (unsigned long long)h);
        if (got != size)
            problem(rel, "size differs");
        else if (strcmp(hex, hash) != 0)
            problem(rel, "contents differ");
        size_t len = strlen(rel);
        if (len > 2 && strcmp(rel + len - 2, ".a") == 0) {
            archives++;
            if (memcmp(head, "!<arch>\n", 8) != 0)
                problem(rel, "not an ar archive");
        } else if (len > 2 && strcmp(rel + len - 2, ".o") == 0) {
            objects++;
            if (memcmp(head, "\177ELF", 4) != 0 || head[4] != 2)
                problem(rel, "not a 64-bit ELF object");
        } else if (len > 2 && strcmp(rel + len - 2, ".h") == 0) {
            headers++;
        }
    }
    fclose(manifest);
    static const char *needed[] = {"/usr/include/stdio.h", "/usr/include/pthread.h", "/usr/lib/libc.a",
                                   "/usr/lib/libm.a", "/usr/lib/crt0.o", "/usr/lib/crt1.o",
                                   "/usr/lib/crti.o", "/usr/lib/crtn.o", "/usr/lib/libpthread.a"};
    for (size_t i = 0; i < sizeof needed / sizeof needed[0]; i++) {
        snprintf(path, sizeof path, "%s%s", root, needed[i]);
        FILE *f = fopen(path, "rb");
        if (!f)
            problem(needed[i], "missing");
        else
            fclose(f);
    }
    if (bad) {
        printf("sysroot-check: %d problems in %d files\n", bad, files);
        return 1;
    }
    printf("sysroot-check: %d files ok (%d headers, %d archives, %d objects)\n", files, headers, archives,
           objects);
    return 0;
}
