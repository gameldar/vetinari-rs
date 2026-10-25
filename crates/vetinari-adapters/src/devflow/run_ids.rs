//! ULIDs for run definitions, from the system clock and the standard
//! library's per-process random hash keys: unique enough for ids, with no
//! dependency for randomness.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use vetinari_core::application::devflow::ports::RunIds;
use vetinari_core::domain::devflow::ids::Ulid;

/// Makes ULIDs at the system's time.
#[derive(Debug, Default)]
pub struct SystemRunIds {
    keys: RandomState,
    count: AtomicU64,
}

impl RunIds for SystemRunIds {
    fn next(&self) -> Ulid {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let millis = u64::try_from(now.as_millis()).unwrap_or(u64::MAX);
        let count = self.count.fetch_add(1, Ordering::Relaxed);
        let half = |salt: u64| {
            let mut hasher = self.keys.build_hasher();
            hasher.write_u64(salt);
            hasher.write_u64(count);
            hasher.write_u128(now.as_nanos());
            u128::from(hasher.finish())
        };
        Ulid::from_parts(millis, (half(1) << 64) | half(2))
    }
}

#[cfg(test)]
mod tests {
    use vetinari_core::domain::devflow::ids::DefinitionId;
    use vetinari_core::domain::devflow::template::TemplateKind;

    use super::*;

    #[test]
    fn each_id_is_new_and_names_a_definition() {
        let ids = SystemRunIds::default();
        let first = ids.next();
        let second = ids.next();
        assert_ne!(first, second);
        assert!(
            DefinitionId::parse(
                DefinitionId::for_run(TemplateKind::FeatureFromTicket, &first).as_str()
            )
            .is_ok()
        );
        let millis = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("after the epoch")
                .as_millis(),
        )
        .expect("fits");
        assert!(
            first.as_str()[..10] >= Ulid::from_parts(millis - 60_000, 0).as_str()[..10],
            "the time comes first"
        );
    }
}
