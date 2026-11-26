// Shapes for the scanner's tests. /* not a block comment */
#pragma once
// No standard header: the checked-in index stays small.

#define AREA(x) ((x) * (x))

namespace geo {
namespace detail {
inline constexpr double kPi = 3.14159;
}  // namespace detail

enum class Color { Red, Green = 2, Blue };

enum Shade {
    Light,
    Dark = 3,
};

struct Point {
    double x;
    double y = 0.0;
};

class Forward;

template <typename T>
class Shape {
public:
    Shape();
    virtual ~Shape();
    virtual T area() const = 0;
    const char* name() const { return "shape {"; }
    void set_sides(int n) { sides_ = n; }

private:
    int sides_;
};

class Circle : public Shape<double>
{
public:
    explicit Circle(double r);
    double area() const override;

private:
    double r_;
};

using Radius = double;
typedef int Count;

template <typename T>
concept Areal = requires(T t) { t.area(); };

double total(const Circle& a, const Circle& b);
}  // namespace geo
