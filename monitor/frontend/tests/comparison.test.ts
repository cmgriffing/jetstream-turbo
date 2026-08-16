import { describe, expect, it } from "vitest";
import { compareCounts } from "../src/lib/comparison";

describe("compareCounts", () => {
  it("reports a positive difference as ahead", () => {
    expect(compareCounts(125, 100)).toEqual({
      position: "ahead",
      difference: 25,
      magnitude: 25,
    });
  });

  it("reports a negative difference as behind", () => {
    expect(compareCounts(75, 100)).toEqual({
      position: "behind",
      difference: -25,
      magnitude: 25,
    });
  });

  it("reports equal counts as even", () => {
    expect(compareCounts(100, 100)).toEqual({
      position: "even",
      difference: 0,
      magnitude: 0,
    });
  });

  it.each([
    [undefined, 10],
    [10, undefined],
    [null, 10],
    [10, null],
    [Number.NaN, 10],
    [10, Number.POSITIVE_INFINITY],
  ])("reports unavailable for missing or non-finite operands (%s, %s)", (subject, reference) => {
    expect(compareCounts(subject, reference)).toEqual({
      position: "unavailable",
      difference: null,
      magnitude: null,
    });
  });

  it("treats real zeroes as available counts", () => {
    expect(compareCounts(0, 7)).toEqual({
      position: "behind",
      difference: -7,
      magnitude: 7,
    });
    expect(compareCounts(0, 0).position).toBe("even");
  });
});
