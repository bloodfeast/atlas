/** Something with an area. */
export interface Shape {
  area(): number;
}

/** A square. */
export class Square implements Shape {
  constructor(public side: number) {}

  area(): number {
    return this.side * this.side;
  }
}

/** Sums the areas. */
export function total(shapes: Shape[]): number {
  let sum = 0;
  for (const s of shapes) {
    sum += s.area();
  }
  return sum;
}
