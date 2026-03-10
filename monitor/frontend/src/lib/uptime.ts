function clampPercent(value: number): number {
  if (!Number.isFinite(value)) return 0;
  return Math.max(0, Math.min(100, value));
}

function getPrecisionDigits(percent: number): number {
  if (percent >= 99.9999) return 6;
  if (percent >= 99.99) return 5;
  if (percent >= 99.9) return 4;
  if (percent >= 99) return 3;
  return 2;
}

interface FormatUptimePercentOptions {
  minimumFractionDigits?: number;
  maximumFractionDigits?: number;
}

export function formatUptimePercent(
  value: number | undefined,
  options: FormatUptimePercentOptions = {},
): string {
  if (typeof value !== "number" || !Number.isFinite(value)) {
    return "--";
  }

  const clamped = clampPercent(value);
  const maximumFractionDigits =
    options.maximumFractionDigits ?? getPrecisionDigits(clamped);
  const minimumFractionDigits = Math.max(
    0,
    Math.min(options.minimumFractionDigits ?? 0, maximumFractionDigits),
  );

  return clamped.toLocaleString(undefined, {
    minimumFractionDigits,
    maximumFractionDigits,
  });
}
