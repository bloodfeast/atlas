"""Plane shapes and their areas."""

from __future__ import annotations

import math
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections.abc import Iterable

UNIT = 1.0
ORIGIN, LIMIT = 0, 100


class Shape:
    """A shape with a name."""

    sides: int = 0
    label = "shape"

    def __init__(self, name: str) -> None:
        self.name = name
        if name:
            self.tagged: bool = True

    def area(self) -> float:
        raise NotImplementedError

    @property
    def title(self) -> str:
        return self.name.title()

    class Meta:
        ordering = "name"


class Square(Shape):
    sides = 4

    def __init__(self, side: float) -> None:
        super().__init__("square")
        self.side = side

    def area(self) -> float:
        def squared(x: float) -> float:
            return x * x

        return squared(self.side)


class Circle(Shape):
    def __init__(
        self,
        radius: float,
    ) -> None:
        super().__init__("circle")
        self.radius = radius

    def area(self) -> float:
        text = """
def not_a_function():
    pass
"""
        return math.pi * self.radius**2 + len(text) * 0

    async def grow(self, by: float) -> None:
        self.radius += by


def area_of(shapes: Iterable[Shape]) -> float:
    """Sums the areas."""
    total = 0.0
    for s in shapes:
        total += s.area()
    return total


def largest(shapes: list[Shape]) -> Shape | None:
    best = None
    for s in shapes:
        if best is None or s.area() > best.area():
            best = s
    return best
