import { describe, expect, it } from 'vitest'
import { extractUptimeRows, normalizeUptimeRow } from '../src/hooks/useStream'

describe('reliability history normalization', () => {
  it('preserves event-time comparison context from versioned history', () => {
    const row = normalizeUptimeRow({
      hour: '2026-08-18 20:00:00',
      reliability_json: JSON.stringify({
        stream_a: {}, stream_b: {}, baseline_1: {}, baseline_2: {},
        event_time: {
          stream_a: { delivery_mode: 'live', source_watermark_us: 10, source_lag_us: 1, event_time_coverage: true, clock_skew_us: 0 },
          stream_b: { delivery_mode: 'catching_up', source_watermark_us: 5, source_lag_us: 6, event_time_coverage: true, clock_skew_us: 0 },
          baseline_1: { delivery_mode: 'unknown', source_watermark_us: null, source_lag_us: null, event_time_coverage: false, clock_skew_us: 0 },
          baseline_2: { delivery_mode: 'unknown', source_watermark_us: null, source_lag_us: null, event_time_coverage: false, clock_skew_us: 0 },
          comparison: { eligible: false, reason: 'catching_up', watermark_skew_us: null },
        },
      }),
      reliability_classification: 'observed',
    })

    expect(row?.reliability?.event_time?.stream_b.delivery_mode).toBe('catching_up')
    expect(row?.reliability?.event_time?.comparison.reason).toBe('catching_up')
  })
  it('preserves prolonged delivery silence separately from transport availability', () => {
    const row = normalizeUptimeRow({
      hour: '2026-07-17 12:00:00',
      reliability_json: JSON.stringify({
        stream_a: { transport_up_seconds: 3600, transport_down_seconds: 0, delivery_up_seconds: 60, delivery_down_seconds: 3540, reconnect_reasons: { data_idle_timeout: 3 }, client_recovery_ms: 15000, coverage: 'observed' },
        stream_b: { transport_up_seconds: 3600, transport_down_seconds: 0, delivery_up_seconds: 3600, delivery_down_seconds: 0, reconnect_reasons: {}, client_recovery_ms: 0, coverage: 'observed' },
        baseline_1: { transport_up_seconds: 3600, transport_down_seconds: 0, delivery_up_seconds: 3600, delivery_down_seconds: 0, reconnect_reasons: {}, client_recovery_ms: 0, coverage: 'observed' },
        baseline_2: { transport_up_seconds: 3600, transport_down_seconds: 0, delivery_up_seconds: 3600, delivery_down_seconds: 0, reconnect_reasons: {}, client_recovery_ms: 0, coverage: 'observed' },
      }),
      reliability_classification: 'observed',
    })
    expect(row?.reliability?.stream_a.transport_up_seconds).toBe(3600)
    expect(row?.reliability?.stream_a.delivery_down_seconds).toBe(3540)
    expect(row?.reliability?.stream_a.reconnect_reasons.data_idle_timeout).toBe(3)
  })

  it('marks legacy rows unknown instead of inventing delivery causes', () => {
    const row = normalizeUptimeRow({ hour: '2026-07-17 11:00:00', stream_a_seconds: 3500 })
    expect(row?.reliability).toBeNull()
    expect(row?.reliability_classification).toBe('legacy_unknown')
  })

  it('preserves transport failures independently from delivery downtime', () => {
    const row = normalizeUptimeRow({
      hour: '2026-07-17 13:00:00',
      reliability: {
        stream_a: { transport_up_seconds: 0, transport_down_seconds: 3600, delivery_up_seconds: 0, delivery_down_seconds: 3600, reconnect_reasons: { handshake_failure: 2 }, client_recovery_ms: 0, coverage: 'observed' },
        stream_b: { transport_up_seconds: 3600, transport_down_seconds: 0, delivery_up_seconds: 3600, delivery_down_seconds: 0, reconnect_reasons: {}, client_recovery_ms: 0, coverage: 'observed' },
        baseline_1: { transport_up_seconds: 3600, transport_down_seconds: 0, delivery_up_seconds: 3600, delivery_down_seconds: 0, reconnect_reasons: {}, client_recovery_ms: 0, coverage: 'observed' },
        baseline_2: { transport_up_seconds: 3600, transport_down_seconds: 0, delivery_up_seconds: 3600, delivery_down_seconds: 0, reconnect_reasons: {}, client_recovery_ms: 0, coverage: 'observed' },
      },
    })
    expect(row?.reliability?.stream_a.transport_down_seconds).toBe(3600)
    expect(row?.reliability?.stream_a.delivery_down_seconds).toBe(3600)
    expect(row?.reliability?.stream_a.reconnect_reasons.handshake_failure).toBe(2)
  })

  it('keeps observed and legacy rows when history coverage is partial', () => {
    const rows = extractUptimeRows({ rows: [
      {
        hour: '2026-07-17 13:00:00',
        reliability: {
          stream_a: { transport_up_seconds: 3600, transport_down_seconds: 0, delivery_up_seconds: 3600, delivery_down_seconds: 0, reconnect_reasons: {}, client_recovery_ms: 0, coverage: 'observed' },
          stream_b: { transport_up_seconds: 3600, transport_down_seconds: 0, delivery_up_seconds: 3600, delivery_down_seconds: 0, reconnect_reasons: {}, client_recovery_ms: 0, coverage: 'observed' },
          baseline_1: { transport_up_seconds: 3600, transport_down_seconds: 0, delivery_up_seconds: 3600, delivery_down_seconds: 0, reconnect_reasons: {}, client_recovery_ms: 0, coverage: 'observed' },
          baseline_2: { transport_up_seconds: 3600, transport_down_seconds: 0, delivery_up_seconds: 3600, delivery_down_seconds: 0, reconnect_reasons: {}, client_recovery_ms: 0, coverage: 'observed' },
        },
      },
      { hour: '2026-07-17 12:00:00', stream_a_seconds: 3590 },
    ] })

    expect(rows).toHaveLength(2)
    expect(rows[0].reliability_classification).toBe('observed')
    expect(rows[1].reliability_classification).toBe('legacy_unknown')
  })
})
