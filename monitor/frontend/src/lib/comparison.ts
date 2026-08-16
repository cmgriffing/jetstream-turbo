export type ComparisonPosition = "ahead" | "behind" | "even" | "unavailable";

export interface CountComparison {
  position: ComparisonPosition;
  difference: number | null;
  magnitude: number | null;
}

export function compareCounts(
  subjectCount: number | null | undefined,
  referenceCount: number | null | undefined,
): CountComparison {
  if (
    typeof subjectCount !== "number" ||
    !Number.isFinite(subjectCount) ||
    typeof referenceCount !== "number" ||
    !Number.isFinite(referenceCount)
  ) {
    return { position: "unavailable", difference: null, magnitude: null };
  }

  const difference = subjectCount - referenceCount;
  const position = difference > 0 ? "ahead" : difference < 0 ? "behind" : "even";

  return { position, difference, magnitude: Math.abs(difference) };
}
