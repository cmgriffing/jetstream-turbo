export type ComparisonPosition = "ahead" | "behind" | "even" | "unavailable";

export interface CountComparison {
  position: ComparisonPosition;
  difference: number | null;
  magnitude: number | null;
}

export type DeliveryMode = "live" | "catching_up" | "unknown";
export type ComparisonReason =
  | "catching_up"
  | "unknown_mode"
  | "missing_event_time_coverage"
  | "watermark_skew"
  | "disconnected"
  | "idle_delivery"
  | "missing_shared_coverage"
  | "incomplete_identity_coverage"
  | "settlement_pending"
  | "legacy_unknown";

export interface EventTimeContext {
  source_watermark_us: number | null;
  delivery_mode: DeliveryMode;
  event_time_coverage: boolean;
}

export interface LiveComparisonEligibility {
  eligible: boolean;
  reason: ComparisonReason | null;
}

export interface BackendPairwiseComparison {
  count_delta: number | null;
  eligible: boolean;
  reason: ComparisonReason | null;
}

export function comparisonFromBackend(
  comparison: BackendPairwiseComparison | null | undefined,
): CountComparison {
  const difference = comparison?.count_delta;
  if (typeof difference !== "number" || !Number.isFinite(difference)) {
    return { position: "unavailable", difference: null, magnitude: null };
  }
  return {
    position: difference > 0 ? "ahead" : difference < 0 ? "behind" : "even",
    difference,
    magnitude: Math.abs(difference),
  };
}

export function eligibilityFromBackend(
  comparison: BackendPairwiseComparison | null | undefined,
): LiveComparisonEligibility {
  return comparison
    ? { eligible: comparison.eligible, reason: comparison.reason }
    : { eligible: false, reason: "legacy_unknown" };
}

export function getLiveComparisonEligibility(
  subject: EventTimeContext | null | undefined,
  reference: EventTimeContext | null | undefined,
  watermarkSkewThresholdUs: number,
): LiveComparisonEligibility {
  if (!subject || !reference) return { eligible: false, reason: "unknown_mode" };
  if (subject.delivery_mode === "catching_up" || reference.delivery_mode === "catching_up") {
    return { eligible: false, reason: "catching_up" };
  }
  if (subject.delivery_mode !== "live" || reference.delivery_mode !== "live") {
    return { eligible: false, reason: "unknown_mode" };
  }
  if (!subject.event_time_coverage || !reference.event_time_coverage) {
    return { eligible: false, reason: "missing_event_time_coverage" };
  }
  if (subject.source_watermark_us === null || reference.source_watermark_us === null) {
    return { eligible: false, reason: "missing_event_time_coverage" };
  }
  if (Math.abs(subject.source_watermark_us - reference.source_watermark_us) > watermarkSkewThresholdUs) {
    return { eligible: false, reason: "watermark_skew" };
  }
  return { eligible: true, reason: null };
}

export function formatComparisonReason(reason: ComparisonReason | null): string {
  switch (reason) {
    case "catching_up": return "catch-up delivery";
    case "unknown_mode": return "delivery mode unknown";
    case "missing_event_time_coverage": return "event-time coverage missing";
    case "watermark_skew": return "source watermarks too far apart";
    case "disconnected": return "stream disconnected";
    case "idle_delivery": return "delivery idle";
    case "missing_shared_coverage": return "shared source window unavailable";
    case "incomplete_identity_coverage": return "portable identity coverage incomplete";
    case "settlement_pending": return "source window settling";
    case "legacy_unknown": return "legacy comparison unavailable";
    default: return "comparable live window";
  }
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
