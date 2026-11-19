/* The heap: many sizes, frees in mixed order, realloc growing and shrinking,
 * calloc zeroing, and a block large enough to be mapped on its own. */
#include <stdint.h>
#include <stdlib.h>
#include "t1.h"

#define SLOTS 4096

static uint32_t state = 12345;
static uint32_t next(void) {
    state = state * 1103515245u + 12345u;
    return state >> 8;
}

int main(void) {
    t1_name = "t1-malloc";
    static unsigned char *blocks[SLOTS];
    static size_t sizes[SLOTS];
    int bad = 0;
    for (int round = 0; round < 40; round++) {
        for (int i = 0; i < SLOTS; i++) {
            int slot = next() % SLOTS;
            if (blocks[slot]) {
                for (size_t j = 0; j < sizes[slot]; j++)
                    if (blocks[slot][j] != (unsigned char)(slot + sizes[slot]))
                        bad++;
                free(blocks[slot]);
                blocks[slot] = NULL;
            } else {
                size_t size = (next() % 8 == 0) ? next() % 65536 : next() % 256;
                blocks[slot] = malloc(size + 1);
                if (!blocks[slot]) {
                    bad++;
                    continue;
                }
                sizes[slot] = size;
                memset(blocks[slot], (unsigned char)(slot + size), size);
            }
        }
    }
    CHECK(bad == 0);
    for (int i = 0; i < SLOTS; i++)
        free(blocks[i]);

    char *grow = NULL;
    for (size_t size = 16; size <= (1 << 22); size *= 2) {
        char *bigger = realloc(grow, size);
        CHECK(bigger != NULL);
        if (!bigger)
            break;
        if (grow == NULL)
            bigger[0] = 'a';
        CHECK(bigger[0] == 'a');
        bigger[size - 1] = 'z';
        grow = bigger;
    }
    grow = realloc(grow, 8);
    CHECK(grow && grow[0] == 'a');
    free(grow);

    unsigned *zeroed = calloc(100000, sizeof *zeroed);
    CHECK(zeroed != NULL);
    if (zeroed) {
        int nonzero = 0;
        for (int i = 0; i < 100000; i++)
            nonzero += zeroed[i] != 0;
        CHECK(nonzero == 0);
        free(zeroed);
    }

    /* 64 MiB: dlmalloc maps this on its own. */
    size_t big_size = 64u << 20;
    char *big = malloc(big_size);
    CHECK(big != NULL);
    if (big) {
        for (size_t i = 0; i < big_size; i += 4096)
            big[i] = (char)i;
        int wrong = 0;
        for (size_t i = 0; i < big_size; i += 4096)
            wrong += big[i] != (char)i;
        CHECK(wrong == 0);
        free(big);
    }

    void *aligned = NULL;
    CHECK(posix_memalign(&aligned, 4096, 10000) == 0 && ((uintptr_t)aligned & 4095) == 0);
    free(aligned);
    return t1_done();
}
