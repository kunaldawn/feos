// t5-cxx-except: C++ exceptions thrown and caught across frames (libc++abi over LLVM's
// libunwind), destructors on the way, a C frame in between, rethrow, exception_ptr,
// exceptions on other threads.
#include <cstdio>
#include <exception>
#include <stdexcept>
#include <string>
#include <thread>

extern "C" int t5_c_frame(int (*callback)(int), int value);

static int destroyed = 0;
struct Guard {
    int n;
    ~Guard() { destroyed++; }
};

struct MyError : std::runtime_error {
    int depth;
    MyError(const std::string &what, int d) : std::runtime_error(what), depth(d) {}
};

[[gnu::noinline]] static int deep(int n) {
    Guard g{n};
    if (n == 0)
        throw MyError("bottom", n);
    return deep(n - 1) + 1;
}

extern "C" int thrower(int v) {
    Guard g{v};
    throw std::out_of_range("from a callback, value " + std::to_string(v));
}

int main() {
    try {
        deep(20);
        std::puts("FAILED: no exception");
        return 1;
    } catch (const std::runtime_error &e) {
        std::printf("caught: %s, %d destructors\n", e.what(), destroyed);
    }
    if (destroyed != 21) {
        std::puts("FAILED: destructors");
        return 1;
    }

    try {
        t5_c_frame(thrower, 21);
    } catch (const std::out_of_range &e) {
        std::printf("through C: %s\n", e.what());
    }

    std::exception_ptr saved;
    try {
        try {
            throw 7;
        } catch (int x) {
            std::printf("inner: %d\n", x);
            throw;
        }
    } catch (...) {
        saved = std::current_exception();
    }
    try {
        std::rethrow_exception(saved);
    } catch (int x) {
        std::printf("rethrown: %d\n", x);
    }

    int caught = 0;
    std::thread t([&] {
        try {
            deep(5);
        } catch (const MyError &e) {
            caught = 1 + e.depth;
        }
    });
    t.join();
    std::printf("on a thread: %d\n", caught);

    try {
        throw std::string("a std::string");
    } catch (const std::string &s) {
        std::printf("caught: %s\n", s.c_str());
    }
    std::puts("t5-cxx-except: ok");
    return 0;
}
