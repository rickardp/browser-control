//! Friendly-name generator for browser instances.
//!
//! Produces names of the form `<kind>-<word>` (e.g. `firefox-pikachu`),
//! falling back to numeric suffixes (`-2`, `-3`, ...) on collision and
//! finally to a different word if a single word's suffix space is exhausted.

use crate::detect::Kind;
use crate::registry::{words::WORDS, Registry};
use anyhow::{anyhow, Result};
use rand::Rng;

/// Maximum number of distinct base words to try before giving up.
const MAX_WORD_ATTEMPTS: usize = 10;
/// Maximum numeric suffix to try per base word.
const MAX_SUFFIX: u32 = 1000;

/// Generate a fresh friendly name for the given kind, avoiding collisions in `registry`.
///
/// Format: `<kind>-<word>`, with `-2`, `-3`, ... appended on collision.
pub fn generate<R: Rng + ?Sized>(kind: Kind, registry: &Registry, rng: &mut R) -> Result<String> {
    let prefix = kind.as_str();

    for _ in 0..MAX_WORD_ATTEMPTS {
        let idx = rng.gen_range(0..WORDS.len());
        let word = WORDS[idx];

        let base = format!("{prefix}-{word}");
        if registry.get_by_name(&base)?.is_none() {
            return Ok(base);
        }

        for n in 2..=MAX_SUFFIX {
            let candidate = format!("{prefix}-{word}-{n}");
            if registry.get_by_name(&candidate)?.is_none() {
                return Ok(candidate);
            }
        }
    }

    Err(anyhow!(
        "failed to generate a unique friendly name for {prefix} after {MAX_WORD_ATTEMPTS} word attempts"
    ))
}

/// Convenience: generate a friendly name using a thread-local RNG.
pub fn generate_default(kind: Kind, registry: &Registry) -> Result<String> {
    let mut rng = rand::thread_rng();
    generate(kind, registry, &mut rng)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::Kind;
    use crate::registry::BrowserRow;
    use rand::rngs::StdRng;
    use rand::SeedableRng;
    use std::collections::HashSet;

    fn placeholder_row(name: &str, kind: Kind) -> BrowserRow {
        BrowserRow {
            name: name.into(),
            kind,
            engine: kind.engine(),
            pid: 0,
            endpoint: "ws://0".into(),
            port: 0,
            profile_dir: std::path::PathBuf::from("/tmp"),
            executable: std::path::PathBuf::from("/bin/true"),
            headless: false,
            started_at: "1970-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn deterministic_with_seed() {
        let reg = Registry::open_in_memory().unwrap();
        let mut rng = StdRng::seed_from_u64(0);

        let first = generate(Kind::Firefox, &reg, &mut rng).unwrap();
        let second = generate(Kind::Firefox, &reg, &mut rng).unwrap();

        assert!(first.starts_with("firefox-"), "got {first}");
        assert!(second.starts_with("firefox-"), "got {second}");
        assert_ne!(first, second);

        // Reproducibility: a fresh RNG with the same seed and a fresh registry
        // must produce the exact same first pick.
        let reg2 = Registry::open_in_memory().unwrap();
        let mut rng2 = StdRng::seed_from_u64(0);
        let first_again = generate(Kind::Firefox, &reg2, &mut rng2).unwrap();
        assert_eq!(first, first_again);
    }

    #[test]
    fn collision_appends_numeric_suffix() {
        // First, compute the deterministic first pick.
        let reg = Registry::open_in_memory().unwrap();
        let mut rng = StdRng::seed_from_u64(42);
        let first_pick = generate(Kind::Chrome, &reg, &mut rng).unwrap();

        // Now seed a fresh registry with that name and re-run with the same RNG seed.
        let reg2 = Registry::open_in_memory().unwrap();
        reg2.insert(&placeholder_row(&first_pick, Kind::Chrome))
            .unwrap();
        let mut rng2 = StdRng::seed_from_u64(42);
        let collided = generate(Kind::Chrome, &reg2, &mut rng2).unwrap();

        assert_eq!(collided, format!("{first_pick}-2"));
    }

    #[test]
    fn many_calls_are_all_unique() {
        let reg = Registry::open_in_memory().unwrap();
        let mut rng = StdRng::seed_from_u64(12345);
        let mut seen: HashSet<String> = HashSet::new();

        for i in 0..200 {
            let name = generate(Kind::Edge, &reg, &mut rng).unwrap();
            assert!(
                seen.insert(name.clone()),
                "duplicate name {name} on iteration {i}"
            );
            reg.insert(&placeholder_row(&name, Kind::Edge)).unwrap();
        }

        assert_eq!(seen.len(), 200);
    }

    #[test]
    fn always_starts_with_kind_prefix() {
        let reg = Registry::open_in_memory().unwrap();
        let mut rng = StdRng::seed_from_u64(7);

        for kind in [
            Kind::Chrome,
            Kind::Edge,
            Kind::Chromium,
            Kind::Brave,
            Kind::Firefox,
        ] {
            let name = generate(kind, &reg, &mut rng).unwrap();
            let expected_prefix = format!("{}-", kind.as_str());
            assert!(
                name.starts_with(&expected_prefix),
                "name {name} does not start with {expected_prefix}"
            );
        }
    }
}
