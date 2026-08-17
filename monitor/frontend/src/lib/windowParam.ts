/**
 * Pure helpers for the `window` analytics-window query parameter.
 *
 * `WINDOW_PARAM_OPTIONS` is the single source of truth for the monitor's
 * analytics windows: URL param value, hours, and toggle label all live in
 * one row, so a new window is added by adding a row here and nowhere else.
 * Decoding is case-insensitive; encoding always produces the canonical form.
 */

export const WINDOW_PARAM_OPTIONS = [
  { param: "24h", hours: 24, label: "24H" },
  { param: "7d", hours: 24 * 7, label: "7D" },
  { param: "28d", hours: 24 * 28, label: "28D" },
] as const;

/**
 * Decode a `window` parameter value into the corresponding window hours.
 * Returns `null` for missing, empty, or unrecognized values.
 */
export function decodeWindowParam(
  raw: string | null | undefined,
): number | null {
  if (!raw) {
    return null;
  }
  const normalized = raw.toLowerCase();
  const option = WINDOW_PARAM_OPTIONS.find((o) => o.param === normalized);
  return option ? option.hours : null;
}

/**
 * Encode window hours into the canonical `window` parameter value.
 * Returns `null` for hours that do not correspond to a known window.
 */
export function encodeWindowParam(hours: number): string | null {
  const option = WINDOW_PARAM_OPTIONS.find((o) => o.hours === hours);
  return option ? option.param : null;
}

/**
 * Return the serialized search string produced by setting `key` to `value`
 * on `search`, preserving every unrelated parameter. The input may include
 * a leading `?`; the output never does.
 */
export function setSearchParam(
  search: string,
  key: string,
  value: string,
): string {
  const params = new URLSearchParams(search);
  params.set(key, value);
  return params.toString();
}
