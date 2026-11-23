// t5-cxx-threads: std::thread on relibc's pthreads, std::mutex, std::condition_variable,
// std::atomic wait/notify (libc++ over feos's futex), thread_local destructors
// (__cxa_thread_atexit), std::async, sleep_for.
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdio>
#include <future>
#include <mutex>
#include <thread>
#include <vector>

static std::atomic<int> tls_destroyed{0};
struct PerThread {
    int id = 0;
    ~PerThread() { tls_destroyed++; }
};
static thread_local PerThread per_thread;

int main() {
    constexpr int threads = 8, rounds = 10000;
    std::mutex m;
    long counter = 0;
    std::vector<std::thread> pool;
    for (int i = 0; i < threads; i++)
        pool.emplace_back([&, i] {
            per_thread.id = i;
            for (int r = 0; r < rounds; r++) {
                std::lock_guard<std::mutex> lock(m);
                counter++;
            }
        });
    for (auto &t : pool)
        t.join();
    std::printf("mutex: %ld of %d\n", counter, threads * rounds);
    std::printf("thread_local destructors: %d\n", tls_destroyed.load());

    std::condition_variable cv;
    bool ready = false;
    int got = 0;
    std::thread consumer([&] {
        std::unique_lock<std::mutex> lock(m);
        cv.wait(lock, [&] { return ready; });
        got = 42;
    });
    std::this_thread::sleep_for(std::chrono::milliseconds(20));
    {
        std::lock_guard<std::mutex> lock(m);
        ready = true;
    }
    cv.notify_one();
    consumer.join();
    std::printf("condition_variable: %d\n", got);

    std::atomic<int> flag{0};
    std::thread waiter([&] {
        flag.wait(0);
        flag.store(2);
        flag.notify_one();
    });
    std::this_thread::sleep_for(std::chrono::milliseconds(20));
    flag.store(1);
    flag.notify_one();
    flag.wait(1);
    waiter.join();
    std::printf("atomic wait: %d\n", flag.load());

    auto start = std::chrono::steady_clock::now();
    std::this_thread::sleep_for(std::chrono::milliseconds(50));
    auto ms = std::chrono::duration_cast<std::chrono::milliseconds>(std::chrono::steady_clock::now() - start).count();
    std::printf("sleep_for: %s\n", ms >= 50 ? "ok" : "short");

    auto f = std::async(std::launch::async, [] { return 6 * 7; });
    std::printf("async: %d\n", f.get());
    std::printf("hardware_concurrency: %s\n", std::thread::hardware_concurrency() > 0 ? "ok" : "0");

    bool ok = counter == threads * rounds && tls_destroyed.load() == threads && got == 42 && flag.load() == 2 && ms >= 50;
    std::puts(ok ? "t5-cxx-threads: ok" : "FAILED");
    return ok ? 0 : 1;
}
