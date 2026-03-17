export type NumericAxisTick = {
  value: number | string;
};

export function readNumericTickValue(value: number | string): number | null {
  const numericValue = typeof value === "number" ? value : Number(value);
  return Number.isFinite(numericValue) ? numericValue : null;
}

function compareNumericTickValues(leftValue: number | string, rightValue: number | string): number {
  const left = readNumericTickValue(leftValue);
  const right = readNumericTickValue(rightValue);

  if (left === null && right === null) return 0;
  if (left === null) return 1;
  if (right === null) return -1;
  return left - right;
}

export function sortNumericAxisTicks<T extends NumericAxisTick>(ticks: T[]): void {
  ticks.sort((left, right) => compareNumericTickValues(left.value, right.value));
}
