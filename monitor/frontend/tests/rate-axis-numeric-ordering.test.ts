import { describe, expect, it } from "vitest";
import { sortNumericAxisTicks } from "../src/components/rateAxis";

describe("sortNumericAxisTicks", () => {
  it("orders numeric string ticks numerically instead of lexicographically", () => {
    const ticks = [{ value: "10" }, { value: "2" }, { value: "1" }];

    sortNumericAxisTicks(ticks);

    expect(ticks.map((tick) => tick.value)).toEqual(["1", "2", "10"]);
  });
});
