//! Constant-memory ingress-ordinal accounting for turbo-fed streams
//! (design D3, D4): a 2^17-slot bit ring classifies each instrumented frame
//! as unique, duplicate, or gap; uninstrumented frames are tallied separately.

use serde::{Deserialize, Serialize};

/// Ring window size in slots: covers out-of-order delivery from turbo's
/// parallel batch broadcast with a large margin; memory stays at 16 KiB.
pub const ORDINAL_RING_SLOTS: u64 = 1 << 17;

/// Cumulative ordinal accounting state carried on every `StreamMessage` for
/// instrumented streams. Totals are monotonic within a monitor process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrdinalAccounting {
    /// Identifier of the turbo process epoch currently in use.
    pub turbo_epoch: String,
    /// Highest observed ingress ordinal in the current epoch.
    pub ordinal_watermark: u64,
    pub unique_total: u64,
    pub duplicate_total: u64,
    /// Synthetic missing-ordinal count (watermark jumps minus the frames).
    pub gap_total: u64,
    /// Frames that arrived without ordinal facts (deploy-window or baseline).
    pub uninstrumented_total: u64,
    /// Cumulative count of turbo epoch changes observed in-band.
    pub epoch_changes: u64,
}

/// Classification of one instrumented frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrdinalClassification {
    Unique,
    Duplicate,
    /// The frame is unique but the watermark jumped, exposing missing
    /// ordinals in the middle.
    Gap { missing: u64 },
}

/// Per-stream derived ordinal picture exposed through health and metrics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrdinalStreamSnapshot {
    pub turbo_epoch: String,
    pub ordinal_watermark: u64,
    pub unique_total: u64,
    pub duplicate_total: u64,
    pub gap_total: u64,
    pub uninstrumented_total: u64,
    pub epoch_changes: u64,
    /// Windowed duplicates / (uniques + duplicates).
    pub duplicate_ratio: f64,
    /// Windowed gaps / (uniques + duplicates + gaps).
    pub gap_rate: f64,
    /// `active` when instrumented frames are flowing in the current window,
    /// `uninstrumented` when the stream is delivering frames without ordinals
    /// (deploy window) or nothing has been instrumented yet.
    pub status: String,
}

impl OrdinalStreamSnapshot {
    pub const ACTIVE: &'static str = "active";
    pub const UNINSTRUMENTED: &'static str = "uninstrumented";
}

/// Threshold configuration for sustained duplicate/gap anomalies (D10).
#[derive(Debug, Clone, Copy)]
pub struct OrdinalThresholds {
    /// Open a duplicate-delivery incident above this windowed ratio.
    pub duplicate_ratio: f64,
    /// Open an ordinal-gap incident above this windowed rate.
    pub gap_rate: f64,
    /// Breaches must persist this long before an incident opens.
    pub sustain: std::time::Duration,
    /// Ratio/rate must stay under threshold this long before resolving.
    pub resolve: std::time::Duration,
}

impl Default for OrdinalThresholds {
    fn default() -> Self {
        Self {
            duplicate_ratio: 0.05,
            gap_rate: 0.005,
            sustain: std::time::Duration::from_secs(60),
            resolve: std::time::Duration::from_secs(300),
        }
    }
}

/// The 2^17-slot bit ring tracking set ordinals over
/// `[highest - ORDINAL_RING_SLOTS, highest]`, in memory only.
pub struct OrdinalRing {
    bits: Vec<u64>,
    highest: Option<u64>,
    epoch: Option<String>,
    unique_total: u64,
    duplicate_total: u64,
    gap_total: u64,
    uninstrumented_total: u64,
    epoch_changes: u64,
}

impl Default for OrdinalRing {
    fn default() -> Self {
        Self::new()
    }
}

impl OrdinalRing {
    pub fn new() -> Self {
        Self {
            bits: vec![0u64; (ORDINAL_RING_SLOTS / 64) as usize],
            highest: None,
            epoch: None,
            unique_total: 0,
            duplicate_total: 0,
            gap_total: 0,
            uninstrumented_total: 0,
            epoch_changes: 0,
        }
    }

    fn reset_bits(&mut self) {
        for word in &mut self.bits {
            *word = 0;
        }
        self.highest = None;
    }

    /// Observe one frame carrying an ingress ordinal and epoch. Returns the
    /// frame's classification.
    pub fn observe(&mut self, ordinal: u64, epoch: &str) -> OrdinalClassification {
        if self.epoch.as_deref() != Some(epoch) {
            let had_epoch = self.epoch.is_some();
            self.reset_bits();
            self.epoch = Some(epoch.to_string());
            // Initialization of the ring is not an epoch change.
            if had_epoch {
                self.epoch_changes = self.epoch_changes.saturating_add(1);
            }
        }
        let slot = ordinal % ORDINAL_RING_SLOTS;
        let word = (slot / 64) as usize;
        let mask = 1u64 << (slot % 64);
        match self.highest {
            None => {
                self.bits[word] |= mask;
                self.highest = Some(ordinal);
                self.unique_total = self.unique_total.saturating_add(1);
                OrdinalClassification::Unique
            }
            Some(h) if ordinal > h => {
                let missing = ordinal - h - 1;
                self.gap_total = self.gap_total.saturating_add(missing);
                self.bits[word] |= mask;
                self.highest = Some(ordinal);
                self.unique_total = self.unique_total.saturating_add(1);
                if missing > 0 {
                    OrdinalClassification::Gap { missing }
                } else {
                    OrdinalClassification::Unique
                }
            }
            Some(h) => {
                if h.saturating_sub(ordinal) >= ORDINAL_RING_SLOTS {
                    // Trailing edge passed this ordinal long ago: classify
                    // unique (acceptable per design D3).
                    self.bits[word] |= mask;
                    self.unique_total = self.unique_total.saturating_add(1);
                    return OrdinalClassification::Unique;
                }
                if self.bits[word] & mask != 0 {
                    self.duplicate_total = self.duplicate_total.saturating_add(1);
                    OrdinalClassification::Duplicate
                } else {
                    self.bits[word] |= mask;
                    self.unique_total = self.unique_total.saturating_add(1);
                    OrdinalClassification::Unique
                }
            }
        }
    }

    /// Observe a frame without ordinal facts (design D4): counted as a unique
    /// frame but only in the uninstrumented tally.
    pub fn observe_uninstrumented(&mut self) {
        self.uninstrumented_total = self.uninstrumented_total.saturating_add(1);
    }

    /// Current cumulative accounting snapshot, if any frame has been observed.
    pub fn snapshot(&self) -> Option<OrdinalAccounting> {
        let epoch = self.epoch.as_ref()?;
        Some(OrdinalAccounting {
            turbo_epoch: epoch.clone(),
            ordinal_watermark: self.highest.unwrap_or(0),
            unique_total: self.unique_total,
            duplicate_total: self.duplicate_total,
            gap_total: self.gap_total,
            uninstrumented_total: self.uninstrumented_total,
            epoch_changes: self.epoch_changes,
        })
    }
}

// (windowed snapshot construction lives in the stats aggregator)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_frame_is_unique_and_sets_watermark() {
        let mut ring = OrdinalRing::new();
        assert_eq!(ring.observe(5, "e1"), OrdinalClassification::Unique);
        let snap = ring.snapshot().unwrap();
        assert_eq!(snap.ordinal_watermark, 5);
        assert_eq!(snap.unique_total, 1);
    }

    #[test]
    fn repeated_ordinal_is_duplicate() {
        let mut ring = OrdinalRing::new();
        ring.observe(1, "e1");
        assert_eq!(ring.observe(2, "e1"), OrdinalClassification::Unique);
        assert_eq!(ring.observe(1, "e1"), OrdinalClassification::Duplicate);
        let snap = ring.snapshot().unwrap();
        assert_eq!(snap.unique_total, 2);
        assert_eq!(snap.duplicate_total, 1);
    }

    #[test]
    fn watermark_jump_counts_missing_ordinals_as_gap() {
        let mut ring = OrdinalRing::new();
        ring.observe(1, "e1");
        assert_eq!(ring.observe(5, "e1"), OrdinalClassification::Gap { missing: 3 });
        let snap = ring.snapshot().unwrap();
        assert_eq!(snap.gap_total, 3);
        assert_eq!(snap.ordinal_watermark, 5);
        assert_eq!(snap.unique_total, 2);
    }

    #[test]
    fn epoch_change_resets_ring_and_counts_marker() {
        let mut ring = OrdinalRing::new();
        ring.observe(100, "e1");
        assert_eq!(ring.observe(1, "e2"), OrdinalClassification::Unique);
        let snap = ring.snapshot().unwrap();
        assert_eq!(snap.turbo_epoch, "e2");
        assert_eq!(snap.ordinal_watermark, 1);
        assert_eq!(snap.epoch_changes, 1);
        // A stale-looking jump within the new epoch is a gap, and the old
        // epoch's watermark no longer matters.
        assert_eq!(
            ring.observe(50, "e2"),
            OrdinalClassification::Gap { missing: 48 }
        );
        assert_eq!(ring.snapshot().unwrap().epoch_changes, 1);
        assert_eq!(ring.snapshot().unwrap().gap_total, 48);
    }

    #[test]
    fn uninstrumented_frames_are_tallied_separately() {
        let mut ring = OrdinalRing::new();
        ring.observe_uninstrumented();
        ring.observe_uninstrumented();
        let state = ring.snapshot();
        // No instrumented frame observed yet: no epoch is known.
        assert!(state.is_none());
        ring.observe(1, "e1");
        let snap = ring.snapshot().unwrap();
        assert_eq!(snap.uninstrumented_total, 2);
        assert_eq!(snap.unique_total, 1);
    }

    #[test]
    fn window_wrap_classifies_very_old_ordinals_as_unique() {
        let mut ring = OrdinalRing::new();
        ring.observe(1, "e1");
        ring.observe(ORDINAL_RING_SLOTS + 5, "e1");
        // ordinal 1 is now outside the ring window: treated as unique again.
        assert_eq!(ring.observe(1, "e1"), OrdinalClassification::Unique);
    }

    #[test]
    fn repeated_ordinal_after_exactly_full_window_is_unique() {
        let mut ring = OrdinalRing::new();
        ring.observe(7, "e1");
        ring.observe(7 + ORDINAL_RING_SLOTS, "e1");
        assert_eq!(
            ring.observe(7, "e1"),
            OrdinalClassification::Unique,
            "distance == window means the bit outside the tracked range reads unique"
        );
    }
}