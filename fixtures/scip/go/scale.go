package shapes

// Double returns a square with twice the side.
func Double(s Square) Square {
	return Square{Side: s.Side * 2}
}
