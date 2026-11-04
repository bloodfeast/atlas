package shapes;

/** Something with an area. */
public interface Shape {
  double area();

  default String describe() {
    return getClass().getSimpleName() + " of area " + area();
  }
}
