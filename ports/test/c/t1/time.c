/* Clocks and sleeping: CLOCK_MONOTONIC moves forward, nanosleep sleeps at least as
 * long as asked, CLOCK_REALTIME is a plausible date, gettimeofday and time agree. */
#include <sys/time.h>
#include <time.h>
#include <unistd.h>
#include "t1.h"

static long long ns(struct timespec t) {
    return (long long)t.tv_sec * 1000000000LL + t.tv_nsec;
}

int main(void) {
    t1_name = "t1-time";
    struct timespec t0, t1, res, req = {0, 200 * 1000 * 1000};
    CHECK(clock_gettime(CLOCK_MONOTONIC, &t0) == 0);
    CHECK(nanosleep(&req, NULL) == 0);
    CHECK(clock_gettime(CLOCK_MONOTONIC, &t1) == 0);
    long long slept = ns(t1) - ns(t0);
    printf("t1-time: slept %lld ns for 200000000\n", slept);
    CHECK(slept >= 200 * 1000 * 1000LL && slept < 5 * 1000 * 1000 * 1000LL);

    CHECK(usleep(50000) == 0);
    struct timespec t2;
    CHECK(clock_gettime(CLOCK_MONOTONIC, &t2) == 0 && ns(t2) - ns(t1) >= 50 * 1000 * 1000LL);

    CHECK(clock_getres(CLOCK_MONOTONIC, &res) == 0 && ns(res) > 0 && ns(res) < 1000 * 1000 * 1000LL);

    struct timespec real;
    CHECK(clock_gettime(CLOCK_REALTIME, &real) == 0);
    /* Without an RTC feos starts at the epoch; with one, after 2020. */
    CHECK(real.tv_sec >= 0 && real.tv_nsec >= 0 && real.tv_nsec < 1000000000L);
    struct timeval tv;
    CHECK(gettimeofday(&tv, NULL) == 0 && tv.tv_sec >= real.tv_sec && tv.tv_sec - real.tv_sec < 5);
    time_t now = time(NULL);
    CHECK(now >= real.tv_sec && now - real.tv_sec < 5);
    struct tm tm;
    CHECK(gmtime_r(&now, &tm) != NULL && tm.tm_year >= 70);
    char date[64];
    CHECK(strftime(date, sizeof date, "%Y-%m-%d %H:%M:%S", &tm) > 0);
    printf("t1-time: realtime %lld s, %s UTC\n", (long long)real.tv_sec, date);

    struct timespec cpu;
    CHECK(clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &cpu) == 0 && ns(cpu) >= 0);
    CHECK(clock_gettime(CLOCK_THREAD_CPUTIME_ID, &cpu) == 0 && ns(cpu) >= 0);
    CHECK(clock_gettime(12345, &cpu) == -1 && errno == EINVAL);
    return t1_done();
}
