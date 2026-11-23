// t5-cxx-hello: libc++'s iostreams, strings and containers.
#include <algorithm>
#include <iomanip>
#include <iostream>
#include <map>
#include <sstream>
#include <string>
#include <vector>

struct Global {
    Global() { std::cout << "static constructor ran" << std::endl; }
    ~Global() { std::cout << "static destructor ran" << std::endl; }
};
static Global global;

int main(int argc, char **argv) {
    std::vector<std::string> args(argv + 1, argv + argc);
    std::cout << "hello from C++ on feos, " << args.size() << " argument(s)" << std::endl;
    for (const auto &a : args)
        std::cout << "arg: " << a << '\n';
    std::map<std::string, int> counts;
    for (const char *w : {"b", "a", "c", "a", "b", "a"})
        counts[w]++;
    std::ostringstream out;
    for (const auto &[k, v] : counts)
        out << k << '=' << v << ' ';
    std::cout << "map: " << out.str() << '\n';
    std::vector<int> v{5, 3, 9, 1, 7};
    std::sort(v.begin(), v.end());
    std::cout << "sorted:";
    for (int x : v)
        std::cout << ' ' << x;
    std::cout << '\n';
    std::cout << "float: " << std::fixed << std::setprecision(3) << 3.14159 << '\n';
    std::cout << "hex: " << std::hex << 48879 << std::dec << '\n';
    std::istringstream in("42 17");
    int a = 0, b = 0;
    in >> a >> b;
    std::cout << "parsed: " << a + b << '\n';
    std::cerr << "to stderr" << std::endl;
    std::cout << "t5-cxx-hello: ok" << std::endl;
    return 0;
}
