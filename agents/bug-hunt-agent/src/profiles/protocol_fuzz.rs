//! `protocol_fuzz`: hand-fuzz protocol decode with random / mutated bytes.
//!
//! Guarantees: `decode_message` / `decode_input` never panic on arbitrary input.
//! Uses seeded RNG (no `cargo fuzz` runtime required for nightly).

use rand::rngs::StdRng;
use rand::{Rng, RngCore, SeedableRng};
use remotelink_protocol::{decode_input, decode_message};
use serde_json::json;

use crate::config::{ChaosProfileConfig, ProfileName, Severity};
use crate::profiles::{ProfileOutcome, ProfileStatus};

/// Well-formed seeds we mutate (increases chance of near-valid inputs).
const SEED_MESSAGES: &[&str] = &[
    r#"{"type":"hello","role":"host","protocol_version":1,"auth":{"device_token":"t"}}"#,
    r#"{"type":"session_accept","session_id":"s","signal_seq":1}"#,
    r#"{"type":"session_offer","session_id":"s","signal_seq":1,"sdp":"v=0","fingerprint_sig":"sig"}"#,
    r#"{"type":"ice_candidate","session_id":"s","signal_seq":1,"candidate":{"candidate":"c"}}"#,
    r#"{"type":"error","code":"x","message":"y"}"#,
    r#"{"client_ts_us":1,"seq":1,"payload":{"kind":"mouse_move","x":0.5,"y":0.5,"display_id":0}}"#,
    r#"{"client_ts_us":1,"seq":1,"payload":{"kind":"key","scancode":28,"extended":false,"pressed":true,"modifiers":0}}"#,
];

/// Run protocol random-byte fuzz profile.
pub fn run_protocol_fuzz(cfg: &ChaosProfileConfig) -> ProfileOutcome {
    let seed = cfg.seed;
    let mut rng = StdRng::seed_from_u64(seed);
    let n = cfg.iterations.max(1);

    let mut pure_random = 0u32;
    let mut mutated = 0u32;
    let mut decoded_ok = 0u32;
    let mut decoded_err = 0u32;

    // Catch panics so a decoder panic becomes a profile Fail, not process abort.
    for i in 0..n {
        let input = if i % 3 == 0 {
            pure_random += 1;
            let len = rng.gen_range(0..=512);
            random_bytes(&mut rng, len)
        } else {
            mutated += 1;
            mutate_seed(&mut rng)
        };

        // Lossy UTF-8: decoder takes &str; invalid sequences become U+FFFD which is fine.
        let s = String::from_utf8_lossy(&input);

        let msg_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = decode_message(&s);
        }));
        let input_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = decode_input(&s);
        }));

        if msg_result.is_err() || input_result.is_err() {
            return ProfileOutcome {
                profile: ProfileName::ProtocolFuzz,
                root_seed: None,
                seed,
                status: ProfileStatus::Fail,
                severity: Some(Severity::High),
                summary: format!("decoder panicked on iteration {i} (len={})", input.len()),
                metrics: json!({
                    "iteration": i,
                    "input_len": input.len(),
                    "pure_random": pure_random,
                    "mutated": mutated,
                }),
                repro: Some(format!(
                    "seed={seed} iteration={i} hex={}",
                    hex_preview(&input, 64)
                )),
            };
        }

        // Track decode success rates (informational).
        match decode_message(&s) {
            Ok(_) => decoded_ok += 1,
            Err(_) => decoded_err += 1,
        }
        let _ = decode_input(&s);
    }

    // Also hit a few fixed adversarial strings.
    for adv in [
        "",
        "{",
        "null",
        "[]",
        "\"str\"",
        &"x".repeat(10_000),
        "{\"type\":null}",
        "\0\0\0",
        "\u{FEFF}{\"type\":\"error\",\"code\":\"c\",\"message\":\"m\"}",
    ] {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = decode_message(adv);
            let _ = decode_input(adv);
        }));
        if r.is_err() {
            return ProfileOutcome {
                profile: ProfileName::ProtocolFuzz,
                root_seed: None,
                seed,
                status: ProfileStatus::Fail,
                severity: Some(Severity::High),
                summary: format!("decoder panicked on adversarial input len={}", adv.len()),
                metrics: json!({ "adversarial": true }),
                repro: Some(format!("adversarial len={}", adv.len())),
            };
        }
    }

    ProfileOutcome {
        profile: ProfileName::ProtocolFuzz,
        root_seed: None,
        seed,
        status: ProfileStatus::Pass,
        severity: None,
        summary: format!(
            "no panics over {n} random/mutated inputs (+adversarial); ok={decoded_ok} err={decoded_err}"
        ),
        metrics: json!({
            "iterations": n,
            "pure_random": pure_random,
            "mutated": mutated,
            "decoded_ok": decoded_ok,
            "decoded_err": decoded_err,
        }),
        repro: Some(format!("seed={seed} iterations={n}")),
    }
}

fn random_bytes(rng: &mut StdRng, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    rng.fill_bytes(&mut buf);
    buf
}

fn mutate_seed(rng: &mut StdRng) -> Vec<u8> {
    let base = SEED_MESSAGES[rng.gen_range(0..SEED_MESSAGES.len())];
    let mut bytes = base.as_bytes().to_vec();
    if bytes.is_empty() {
        return random_bytes(rng, 16);
    }
    let mutations = rng.gen_range(1..=8);
    for _ in 0..mutations {
        match rng.gen_range(0..4) {
            0 => {
                // Flip a byte.
                let i = rng.gen_range(0..bytes.len());
                bytes[i] ^= 1u8 << rng.gen_range(0..8u8);
            }
            1 => {
                // Insert random byte.
                let i = rng.gen_range(0..=bytes.len());
                bytes.insert(i, rng.gen());
            }
            2 => {
                // Delete a byte.
                if !bytes.is_empty() {
                    let i = rng.gen_range(0..bytes.len());
                    bytes.remove(i);
                }
            }
            _ => {
                // Splice random chunk.
                let i = rng.gen_range(0..=bytes.len());
                let chunk_len = rng.gen_range(1..=16);
                let chunk = random_bytes(rng, chunk_len);
                for (off, b) in chunk.into_iter().enumerate() {
                    bytes.insert(i + off, b);
                }
            }
        }
    }
    bytes
}

fn hex_preview(bytes: &[u8], max: usize) -> String {
    bytes
        .iter()
        .take(max)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_fuzz_never_panics() {
        let cfg = ChaosProfileConfig {
            seed: 12345,
            iterations: 128,
            ..ChaosProfileConfig::default()
        };
        let o = run_protocol_fuzz(&cfg);
        assert_eq!(o.status, ProfileStatus::Pass, "{}", o.summary);
    }

    #[test]
    fn deterministic_metrics() {
        let cfg = ChaosProfileConfig {
            seed: 42,
            iterations: 40,
            ..ChaosProfileConfig::default()
        };
        let a = run_protocol_fuzz(&cfg);
        let b = run_protocol_fuzz(&cfg);
        assert_eq!(a.metrics, b.metrics);
    }
}
