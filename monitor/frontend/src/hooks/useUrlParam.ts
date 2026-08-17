import { useCallback, useEffect, useState } from "react";
import { setSearchParam } from "@/lib/windowParam";

export interface UseUrlParamOptions<T> {
  /** Query parameter name, e.g. `"window"`. */
  key: string;
  /** Fallback value used when the parameter is missing or invalid. */
  defaultValue: T;
  /** Serialize a value to its canonical URL form, or `null` if unrepresentable. */
  encode: (value: T) => string | null;
  /** Parse a raw parameter value (case-insensitive where appropriate), or `null` if invalid. */
  decode: (raw: string | null) => T | null;
}

export type UrlParamSetter<T> = (value: T) => void;

function buildUrl(key: string, canonicalValue: string): string {
  const nextSearch = setSearchParam(
    window.location.search,
    key,
    canonicalValue,
  );
  return `${window.location.pathname}?${nextSearch}${window.location.hash}`;
}

/**
 * Two-way binding between component state and a single query parameter.
 *
 * - On mount: reads the parameter, decodes it, falls back to `defaultValue`
 *   when missing or invalid, and canonicalizes the URL — invalid values are
 *   rewritten to the canonical default and non-canonical values to their
 *   canonical form — so any copied URL round-trips to the same view.
 * - Setter: serializes the value, updates the URL via `history.replaceState`
 *   preserving unrelated query parameters, then updates state. Because
 *   `replaceState` is used, no history entries are added and back/forward
 *   does not step through previously selected values.
 */
export function useUrlParam<T>({
  key,
  defaultValue,
  encode,
  decode,
}: UseUrlParamOptions<T>): [T, UrlParamSetter<T>] {
  const [value, setValue] = useState<T>(() => {
    const raw = new URLSearchParams(window.location.search).get(key);
    const decoded = decode(raw);
    return decoded === null ? defaultValue : decoded;
  });

  useEffect(() => {
    const params = new URLSearchParams(window.location.search);
    const raw = params.get(key);
    const decoded = decode(raw);

    if (decoded === null) {
      const canonical = encode(defaultValue);
      if (canonical !== null) {
        history.replaceState(null, "", buildUrl(key, canonical));
      }
      return;
    }

    const canonical = encode(decoded);
    if (canonical !== null && canonical !== raw) {
      history.replaceState(null, "", buildUrl(key, canonical));
    }
  }, [key, defaultValue, encode, decode]);

  const set = useCallback(
    (next: T) => {
      const canonical = encode(next);
      if (canonical !== null) {
        history.replaceState(null, "", buildUrl(key, canonical));
      }
      setValue(next);
    },
    [key, encode],
  );

  return [value, set];
}
