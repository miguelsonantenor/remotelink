//! Near-zero energy helpers for exclusive-mode loopback detection.

/// Peak absolute sample threshold treated as near-silence (i16 PCM).
///
/// Well below normal tone/game audio; above true digital zero so dither and
/// residual DAC noise do not prevent silence classification.
pub const NEAR_SILENCE_PEAK: i16 = 32;

/// Returns true when every sample's absolute value is ≤ `peak_threshold`.
///
/// Empty buffers are treated as silent. Used by stub and (future) native
/// capture so exclusive-mode warnings fire on real near-zero PCM, not only
/// test inject APIs.
pub fn pcm_is_near_silence(pcm: &[i16], peak_threshold: i16) -> bool {
    if pcm.is_empty() {
        return true;
    }
    let thr = peak_threshold.unsigned_abs();
    pcm.iter().all(|&s| s.unsigned_abs() <= thr)
}

/// Convenience: [`NEAR_SILENCE_PEAK`] threshold.
pub fn pcm_is_near_silence_default(pcm: &[i16]) -> bool {
    pcm_is_near_silence(pcm, NEAR_SILENCE_PEAK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zeros_are_silent() {
        assert!(pcm_is_near_silence_default(&[0, 0, 0]));
    }

    #[test]
    fn tone_is_not_silent() {
        assert!(!pcm_is_near_silence_default(&[0, 1000, -1000]));
    }

    #[test]
    fn below_threshold_counts_silent() {
        assert!(pcm_is_near_silence(&[10, -10, 32, -32], 32));
        assert!(!pcm_is_near_silence(&[10, 33], 32));
    }
}
