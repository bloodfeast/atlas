#include "shapes.hpp"

namespace geo {
static const char* kRaw = R"(class NotAClass { int x; };)";

Circle::Circle(double r) : r_(r) {}

double Circle::area() const
{
    return detail::kPi * r_ * r_;
}

double total(const Circle& a, const Circle& b) {
#if defined(FAST)
    return a.area();
#else
    return a.area() + b.area();
#endif
}
}  // namespace geo

int main() {
    return 0;
}
