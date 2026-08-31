import { describe, expect, it } from "vitest";
import { normalizeUptimeRow } from "../src/hooks/useStream";

const legacyRow = {
  hour: "2026-01-01 01:00:00",
  stream_a_seconds: 3000,
  stream_b_seconds: 2900,
  stream_a_downtime_seconds: 600,
  stream_b_downtime_seconds: 700,
  // Legacy rows count every failed status, including handshake attempts.
  stream_a_disconnects: 5,
  stream_b_disconnects: 7,
  stream_a_messages: 1000,
  stream_b_messages: 900,
  metrics_contract_version: 3,
  reliability_contract_version: 3,
  reliability_classification: "observed",
};

const episodeRow = {
  hour: "2026-01-01 02:00:00",
  stream_a_seconds: 3500,
  stream_b_seconds: 3400,
  stream_a_downtime_seconds: 100,
  stream_b_downtime_seconds: 200,
  // Under v4 these legacy columns are not used for episodes.
  stream_a_disconnects: 0,
  stream_b_disconnects: 0,
  stream_a_outage_episodes: 1,
  stream_b_outage_episodes: 1,
  stream_a_reconnect_attempts: 4,
  stream_b_reconnect_attempts: 2,
  stream_a_messages: 5000,
  stream_b_messages: 4900,
  metrics_contract_version: 4,
  reliability_contract_version: 4,
  reliability_classification: "observed_episodes",
};

describe("hourly reliability rows", () => {
  it("keeps legacy disconnect-attempt classification without episode fields", () => {
    const row = normalizeUptimeRow(legacyRow);
    expect(row).not.toBeNull();
    expect(row!.reliability_classification).toBe("observed");
    expect(row!.stream_a_disconnects).toBe(5);
    expect(row!.stream_a_outage_episodes).toBeUndefined();
    expect(row!.stream_a_reconnect_attempts).toBeUndefined();
  });

  it("reads v4 episode and attempt counters without using legacy disconnect columns", () => {
    const row = normalizeUptimeRow(episodeRow);
    expect(row).not.toBeNull();
    expect(row!.reliability_classification).toBe("observed_episodes");
    expect(row!.stream_a_outage_episodes).toBe(1);
    expect(row!.stream_b_outage_episodes).toBe(1);
    expect(row!.stream_a_reconnect_attempts).toBe(4);
    expect(row!.stream_b_reconnect_attempts).toBe(2);
  });

  it("never merges legacy disconnect attempts into v4 episode counts", () => {
    const legacy = normalizeUptimeRow(legacyRow)!;
    const episodes = normalizeUptimeRow(episodeRow)!;
    // Aggregating different contract generations is forbidden.
    const mixedEpisodes =
      (legacy.stream_a_outage_episodes ?? 0) + (episodes.stream_a_outage_episodes ?? 0);
    expect(mixedEpisodes).toBe(episodes.stream_a_outage_episodes);
    expect(legacy.stream_a_outage_episodes).toBeUndefined();
  });
});
