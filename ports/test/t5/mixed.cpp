// t5-mixed: a C++ program statically linked with a Rust static library by clang++:
// calls both ways, a C++ exception through a Rust frame, a Rust panic through a C++
// frame, Rust threads and C++ iostreams in one program, one libunwind.
#include <cstdint>
#include <iostream>
#include <stdexcept>
#include <string>

extern "C" {
int64_t rust_sum(const int64_t *values, size_t n);
void rust_through(int value);
int rust_catch();
size_t rust_drops();
uint64_t rust_thread_sum();
}

static int cxx_destroyed = 0;
struct CxxGuard {
    ~CxxGuard() { cxx_destroyed++; }
};

extern "C" void cxx_throw(int value) {
    throw std::runtime_error("C++ exception " + std::to_string(value));
}

extern "C" int cxx_call(int (*callback)(int), int value) {
    CxxGuard g;
    return callback(value) + 1;
}

int main() {
    int64_t values[] = {1, 2, 3, 4, 5};
    std::cout << "rust_sum: " << rust_sum(values, 5) << std::endl;
    try {
        rust_through(9);
        std::cout << "FAILED: no exception" << std::endl;
        return 1;
    } catch (const std::runtime_error &e) {
        std::cout << "through Rust: " << e.what() << ", rust drops " << rust_drops() << std::endl;
    }
    int caught = rust_catch();
    std::cout << "rust_catch: " << caught << ", C++ destructors " << cxx_destroyed
              << ", rust drops " << rust_drops() << std::endl;
    std::cout << "rust threads: " << rust_thread_sum() << std::endl;
    bool ok = caught == 1 && cxx_destroyed == 1 && rust_drops() == 2;
    std::cout << (ok ? "t5-mixed: ok" : "FAILED") << std::endl;
    return ok ? 0 : 1;
}
