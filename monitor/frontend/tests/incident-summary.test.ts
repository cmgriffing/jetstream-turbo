import { describe, expect, it } from "vitest";
import {
  activeIncidents,
  buildIncidentsUrl,
  incidentDetailUrl,
  parseIncidentList,
} from "../src/lib/incidents";

const api = {
  data: {
    next_cursor: null,
    incidents: [
      {
        id: "01ARZ3NDEKTSV4RRFFQ69G5FAZ",
        stream_id: "a",
        state: "open",
        trigger: "delivery_idle",
        detected_at: "2026-01-01T00:00:00Z",
        resolved_at: null,
        transport_recovered_at: null,
        total_silence_ms: null,
        detected_recovery_ms: null,
        reconnect_attempts: 2,
        connection_epoch: 3,
        observation_complete: true,
        monitor_release: "0.1.0",
      },
      {
        id: "01ARZ3NDEKTSV4RRFFQ69G5FBX",
        stream_id: "b",
        state: "resolved",
        trigger: "transport_loss",
        detected_at: "2026-01-01T00:01:00Z",
        resolved_at: "2026-01-01T00:05:00Z",
        total_silence_ms: 240000,
        detected_recovery_ms: 230000,
        reconnect_attempts: 0,
        connection_epoch: 1,
        observation_complete: true,
        monitor_release: "0.1.0",
      },
    ],
  },
};

describe("incident summary", () => {
  it("builds versioned list and detail URLs", () => {
    expect(buildIncidentsUrl("http://mon:3001")).toBe(
      "http://mon:3001/api/v1/incidents?limit=10",
    );
    expect(incidentDetailUrl("http://mon:3001/", "01ARZ3NDEKTSV4RRFFQ69G5FAZ")).toBe(
      "http://mon:3001/api/v1/incidents/01ARZ3NDEKTSV4RRFFQ69G5FAZ",
    );
  });

  it("parses sanitized incident summaries and finds active ones", () => {
    const incidents = parseIncidentList(api);
    expect(incidents).toHaveLength(2);
    expect(activeIncidents(incidents)).toHaveLength(1);
    expect(activeIncidents(incidents)[0].trigger).toBe("delivery_idle");
  });

  it("rejects malformed responses without throwing", () => {
    expect(parseIncidentList(null)).toEqual([]);
    expect(parseIncidentList({})).toEqual([]);
    expect(parseIncidentList({ data: {} })).toEqual([]);
  });

  it("renders degraded monitor semantics only through data states", () => {
    // Health is rendered via status fields; the summary must not invent episodes.
    const episodes = parseIncidentList(api).map((incident) => incident.reconnect_attempts);
    expect(episodes).toEqual([2, 0]);
  });
});
