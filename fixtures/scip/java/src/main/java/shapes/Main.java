package shapes;

/** Prints some areas. */
public class Main {
  public static void main(String[] args) {
    Shape s = Square.unit();
    System.out.println(s.describe());
    Shape half =
        new Shape() {
          @Override
          public double area() {
            return 0.5;
          }
        };
    System.out.println(half.area() + new Square(2).area());
  }
}
