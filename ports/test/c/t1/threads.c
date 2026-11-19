/* pthreads over THREAD_CREATE and futexes: create and join with return values, a
 * mutex-protected counter, a condition variable handing work over, thread-local
 * variables, detached threads and a caller-supplied stack size. */
#include <pthread.h>
#include <stdint.h>
#include <stdlib.h>
#include <unistd.h>
#include "t1.h"

#define THREADS 8
#define ROUNDS 20000

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t ready = PTHREAD_COND_INITIALIZER;
static long counter;
static int queue[16], queued, consumed_sum, producer_done;
static __thread int tls_value = 7;
static int tls_ok[THREADS];

static void *adder(void *arg) {
    int id = (int)(intptr_t)arg;
    tls_value = 100 + id;
    for (int i = 0; i < ROUNDS; i++) {
        pthread_mutex_lock(&lock);
        counter++;
        pthread_mutex_unlock(&lock);
        if (i % 1000 == 0)
            sched_yield();
    }
    tls_ok[id] = tls_value == 100 + id;
    return (void *)(intptr_t)(id * 10);
}

static void *consumer(void *arg) {
    (void)arg;
    int sum = 0;
    pthread_mutex_lock(&lock);
    for (;;) {
        while (queued == 0 && !producer_done)
            pthread_cond_wait(&ready, &lock);
        if (queued == 0 && producer_done)
            break;
        sum += queue[--queued];
        pthread_cond_signal(&ready);
    }
    consumed_sum = sum;
    pthread_mutex_unlock(&lock);
    return NULL;
}

static volatile int detached_ran;
static void *detached(void *arg) {
    (void)arg;
    detached_ran = 1;
    return NULL;
}

int main(void) {
    t1_name = "t1-threads";
    pthread_t threads[THREADS];
    for (int i = 0; i < THREADS; i++)
        CHECK(pthread_create(&threads[i], NULL, adder, (void *)(intptr_t)i) == 0);
    for (int i = 0; i < THREADS; i++) {
        void *ret = NULL;
        CHECK(pthread_join(threads[i], &ret) == 0);
        CHECK((intptr_t)ret == i * 10);
        CHECK(tls_ok[i]);
    }
    CHECK(counter == (long)THREADS * ROUNDS);
    CHECK(tls_value == 7);
    printf("t1-threads: counter %ld\n", counter);

    pthread_t c;
    counter = 0;
    CHECK(pthread_create(&c, NULL, consumer, NULL) == 0);
    int expected = 0;
    for (int i = 1; i <= 1000; i++) {
        pthread_mutex_lock(&lock);
        while (queued == 16)
            pthread_cond_wait(&ready, &lock);
        queue[queued++] = i;
        expected += i;
        pthread_cond_signal(&ready);
        pthread_mutex_unlock(&lock);
    }
    pthread_mutex_lock(&lock);
    producer_done = 1;
    pthread_cond_broadcast(&ready);
    pthread_mutex_unlock(&lock);
    CHECK(pthread_join(c, NULL) == 0);
    CHECK(consumed_sum == expected);
    printf("t1-threads: condvar sum %d\n", consumed_sum);

    pthread_attr_t attr;
    pthread_t d;
    CHECK(pthread_attr_init(&attr) == 0);
    CHECK(pthread_attr_setstacksize(&attr, 256 * 1024) == 0);
    CHECK(pthread_attr_setdetachstate(&attr, PTHREAD_CREATE_DETACHED) == 0);
    CHECK(pthread_create(&d, &attr, detached, NULL) == 0);
    for (int i = 0; i < 200 && !detached_ran; i++)
        usleep(10000);
    CHECK(detached_ran);
    pthread_attr_destroy(&attr);

    CHECK(pthread_equal(pthread_self(), pthread_self()));
    return t1_done();
}
