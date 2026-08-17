import { describe, expect, it } from "vitest";
import {
  decodeWindowParam,
  encodeWindowParam,
  setSearchParam,
  WINDOW_PARAM_OPTIONS,
} from "../src/lib/windowParam";

describe("encodeWindowParam", () => {
  it("encodes every registry window to its canonical param", () => {
    for (const option of WINDOW_PARAM_OPTIONS) {
      expect(encodeWindowParam(option.hours)).toBe(option.param);
    }
  });

  it("returns null for unknown hours", () => {
    expect(encodeWindowParam(0)).toBeNull();
    expect(encodeWindowParam(48)).toBeNull();
    expect(encodeWindowParam(24 * 30)).toBeNull();
  });

  it("round-trips through decode", () => {
    for (const option of WINDOW_PARAM_OPTIONS) {
      const encoded = encodeWindowParam(option.hours);
      expect(encoded).toBe(option.param);
      expect(decodeWindowParam(encoded)).toBe(option.hours);
    }
  });
});

describe("decodeWindowParam", () => {
  it("decodes canonical values to window hours", () => {
    for (const option of WINDOW_PARAM_OPTIONS) {
      expect(decodeWindowParam(option.param)).toBe(option.hours);
    }
  });

  it("decodes case variants to the matching window", () => {
    expect(decodeWindowParam("24H")).toBe(24);
    expect(decodeWindowParam("7D")).toBe(24 * 7);
    expect(decodeWindowParam("28D")).toBe(24 * 28);
  });

  it("returns null for missing or invalid values", () => {
    expect(decodeWindowParam(null)).toBeNull();
    expect(decodeWindowParam(undefined)).toBeNull();
    expect(decodeWindowParam("")).toBeNull();
    expect(decodeWindowParam("99h")).toBeNull();
    expect(decodeWindowParam("week")).toBeNull();
    expect(decodeWindowParam(" 7d")).toBeNull();
  });
});

describe("setSearchParam", () => {
  it("updates an existing parameter in place", () => {
    expect(setSearchParam("source=incident&window=7d", "window", "28d")).toBe(
      "source=incident&window=28d",
    );
  });

  it("adds a parameter while preserving unrelated ones", () => {
    expect(setSearchParam("source=incident", "window", "24h")).toBe(
      "source=incident&window=24h",
    );
  });

  it("creates a query string from an empty search", () => {
    expect(setSearchParam("", "window", "24h")).toBe("window=24h");
  });

  it("tolerates a leading question mark", () => {
    expect(setSearchParam("?source=incident", "window", "7d")).toBe(
      "source=incident&window=7d",
    );
  });
});
