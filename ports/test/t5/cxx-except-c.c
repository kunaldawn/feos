/* A C frame for t5-cxx-except to throw through: clang gives C asynchronous unwind
 * tables on feos by default. */
int t5_c_frame(int (*callback)(int), int value) {
    volatile int local = value * 2;
    return callback(local) + 1;
}
