from shapes.geometry import Circle, Square, area_of, largest


def main() -> None:
    shapes = [Square(2.0), Circle(1.0)]
    print(area_of(shapes))
    big = largest(shapes)
    if big is not None:
        print(big.title, big.area())


if __name__ == "__main__":
    main()
