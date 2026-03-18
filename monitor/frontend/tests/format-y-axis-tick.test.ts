import { describe, expect, it } from "vitest";
import { formatAdaptiveYAxisTick } from "../src/components/Charts";

describe("formatAdaptiveYAxisTick", () => {
  // When min=0, max=60, decimals=0 (integer range)
  const intMin = 0;
  const intMax = 60;
  const suffix = "/s";

  it("preserves trailing zeros in integer values", () => {
    expect(formatAdaptiveYAxisTick(60, intMin, intMax, suffix)).toBe("60/s");
    expect(formatAdaptiveYAxisTick(50, intMin, intMax, suffix)).toBe("50/s");
    expect(formatAdaptiveYAxisTick(40, intMin, intMax, suffix)).toBe("40/s");
    expect(formatAdaptiveYAxisTick(10, intMin, intMax, suffix)).toBe("10/s");
    expect(formatAdaptiveYAxisTick(100, intMin, intMax, suffix)).toBe("100/s");
    expect(formatAdaptiveYAxisTick(200, intMin, intMax, suffix)).toBe("200/s");
  });

  it("strips trailing zeros after decimal point", () => {
    // When min=0, max=1, decimals=1 (decimal range)
    const decMin = 0;
    const decMax = 1;
    expect(formatAdaptiveYAxisTick(0.5, decMin, decMax, suffix)).toBe("0.5/s");
    expect(formatAdaptiveYAxisTick(1.0, decMin, decMax, suffix)).toBe("1/s");
  });

  it("handles zero correctly", () => {
    expect(formatAdaptiveYAxisTick(0, intMin, intMax, suffix)).toBe("0/s");
  });

  it("handles string tick values", () => {
    expect(formatAdaptiveYAxisTick("50", intMin, intMax, suffix)).toBe("50/s");
    expect(formatAdaptiveYAxisTick("60", intMin, intMax, suffix)).toBe("60/s");
  });
});
