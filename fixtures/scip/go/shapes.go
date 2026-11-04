// Package shapes is a fixture for atlas's SCIP decoder tests.
package shapes

// Shape has an area.
type Shape interface {
	Area() float64
}

// Square is a Shape.
type Square struct {
	Side float64
}

// Area returns the square's area.
func (s Square) Area() float64 {
	return s.Side * s.Side
}

// Total sums the areas.
func Total(shapes []Shape) float64 {
	sum := 0.0
	for _, s := range shapes {
		sum += s.Area()
	}
	return sum
}

// Unit is the side of the unit square.
const Unit = 1.0
