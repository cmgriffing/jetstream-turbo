import { describe, expect, it } from "vitest";
import {
  compareCounts,
  comparisonFromBackend,
  eligibilityFromBackend,
  getLiveComparisonEligibility,
} from "../src/lib/comparison";

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

describe("backend-authored source-window comparisons", () => {
  it("renders the backend delta without subtracting raw arrival totals", () => {
    const backend = { eligible: true, reason: null, count_delta: -3 } as const;
    expect(comparisonFromBackend(backend)).toEqual({
      position: "behind",
      difference: -3,
      magnitude: 3,
    });
    expect(eligibilityFromBackend(backend)).toEqual({ eligible: true, reason: null });
  });

  it.each([
    "catching_up",
    "settlement_pending",
    "disconnected",
    "incomplete_identity_coverage",
    "missing_shared_coverage",
  ] as const)("preserves the backend ineligibility reason %s", (reason) => {
    const backend = { eligible: false, reason, count_delta: null };
    expect(comparisonFromBackend(backend).position).toBe("unavailable");
    expect(eligibilityFromBackend(backend).reason).toBe(reason);
  });

  it("marks missing legacy backend objects unknown", () => {
    expect(eligibilityFromBackend(undefined)).toEqual({
      eligible: false,
      reason: "legacy_unknown",
    });
  });
});

describe("getLiveComparisonEligibility", () => {
  const live = {
    source_watermark_us: 10_000_000,
    delivery_mode: "live" as const,
    event_time_coverage: true,
  };

  it("allows overlapping covered live windows", () => {
    expect(getLiveComparisonEligibility(live, { ...live, source_watermark_us: 12_000_000 }, 5_000_000)).toEqual({
      eligible: true,
      reason: null,
    });
  });

  it("suppresses a catching-up stream", () => {
    expect(getLiveComparisonEligibility({ ...live, delivery_mode: "catching_up" }, live, 5_000_000).reason).toBe("catching_up");
  });

  it("suppresses missing event-time coverage", () => {
    expect(getLiveComparisonEligibility({ ...live, event_time_coverage: false }, live, 5_000_000).reason).toBe("missing_event_time_coverage");
  });

  it("suppresses excessive source-watermark skew", () => {
    expect(getLiveComparisonEligibility(live, { ...live, source_watermark_us: 20_000_000 }, 5_000_000).reason).toBe("watermark_skew");
  });

  it("treats absent legacy context as unknown", () => {
    expect(getLiveComparisonEligibility(undefined, live, 5_000_000).reason).toBe("unknown_mode");
  });
});
