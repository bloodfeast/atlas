package shapes;

import java.util.Comparator;

/** A square. */
public final class Square
    implements Shape, Comparable<Square> {
  public static final Comparator<Square> BY_SIDE =
      new Comparator<Square>() {
        @Override
        public int compare(Square a, Square b) {
          return Double.compare(a.side, b.side);
        }
      };

  private final double side;
  int corners = 4, edges = 4;

  public Square(double side) {
    this.side = side;
  }

  @Override
  public double area() {
    return side * side;
  }

  @Override
  public int compareTo(Square other) {
    return BY_SIDE.compare(this, other);
  }

  public enum Kind {
    SMALL,
    LARGE;

    public boolean big() {
      return this == LARGE;
    }
  }

  public static Square unit()
      throws IllegalStateException {
    return new Square(1);
  }
}
