// t5-cxx-fs DIR: std::filesystem: directories, iteration, sizes, copy, rename, removal.
#include <algorithm>
#include <cstdio>
#include <filesystem>
#include <fstream>
#include <string>
#include <vector>

namespace fs = std::filesystem;

int main(int argc, char **argv) {
    if (argc != 2) {
        std::fprintf(stderr, "usage: t5-cxx-fs DIR\n");
        return 2;
    }
    fs::path dir = argv[1];
    std::error_code ec;
    fs::remove_all(dir, ec);
    fs::create_directories(dir / "a" / "b");
    for (const char *name : {"one.txt", "two.txt", "a/three.txt", "a/b/four.txt"}) {
        std::ofstream(dir / name) << "contents of " << name << '\n';
    }
    std::vector<std::string> top;
    for (const auto &e : fs::directory_iterator(dir))
        top.push_back(e.path().filename().string() + (e.is_directory() ? "/" : ""));
    std::sort(top.begin(), top.end());
    std::printf("directory_iterator:");
    for (const auto &s : top)
        std::printf(" %s", s.c_str());
    std::printf("\n");

    std::vector<std::string> all;
    for (const auto &e : fs::recursive_directory_iterator(dir))
        all.push_back(fs::relative(e.path(), dir).generic_string());
    std::sort(all.begin(), all.end());
    std::printf("recursive:");
    for (const auto &s : all)
        std::printf(" %s", s.c_str());
    std::printf("\n");

    std::printf("file_size: %ju\n", (uintmax_t)fs::file_size(dir / "one.txt"));
    fs::copy_file(dir / "one.txt", dir / "copy.txt");
    fs::rename(dir / "two.txt", dir / "a" / "moved.txt");
    std::printf("exists: copy %d, moved %d, old %d\n", fs::exists(dir / "copy.txt"),
                fs::exists(dir / "a" / "moved.txt"), fs::exists(dir / "two.txt"));
    std::ifstream in(dir / "copy.txt");
    std::string line;
    std::getline(in, line);
    std::printf("copy: %s\n", line.c_str());
    std::printf("is_directory: %d, absolute: %d\n", fs::is_directory(dir / "a"),
                fs::absolute(dir).is_absolute());
    auto removed = fs::remove_all(dir);
    std::printf("remove_all: %ju, exists %d\n", (uintmax_t)removed, fs::exists(dir));
    try {
        fs::file_size(dir / "missing");
    } catch (const fs::filesystem_error &e) {
        std::printf("filesystem_error: %s\n", e.code() == std::errc::no_such_file_or_directory ? "ENOENT" : "other");
    }
    std::puts("t5-cxx-fs: ok");
    return 0;
}
