import { Square, total } from "./shapes";

export const UNIT = new Square(1);

export function report(): string {
  return `total ${total([UNIT, new Square(2)])}`;
}
