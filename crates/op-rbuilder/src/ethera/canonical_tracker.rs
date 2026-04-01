use alloy_primitives::B256;
use std::collections::{BTreeSet, HashMap};

#[derive(Debug, Default)]
pub struct XtCanonicalTracker {
    cumulative_instance_ids_by_block: HashMap<B256, Vec<String>>,
}

impl XtCanonicalTracker {
    pub fn record_candidate(
        &mut self,
        block_hash: B256,
        cumulative_instance_ids: &BTreeSet<String>,
    ) {
        if cumulative_instance_ids.is_empty() {
            return;
        }

        self.cumulative_instance_ids_by_block.insert(
            block_hash,
            cumulative_instance_ids.iter().cloned().collect(),
        );
    }

    pub fn take_confirmed(&mut self, block_hash: B256) -> Vec<String> {
        self.cumulative_instance_ids_by_block
            .remove(&block_hash)
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::XtCanonicalTracker;
    use alloy_primitives::B256;
    use std::collections::BTreeSet;

    #[test]
    fn xt_canonical_tracker_returns_cumulative_ids_for_committed_block() {
        let mut tracker = XtCanonicalTracker::default();
        let first_hash = B256::repeat_byte(0x11);
        let second_hash = B256::repeat_byte(0x22);

        let mut cumulative = BTreeSet::new();
        cumulative.insert("xt-1".to_string());
        tracker.record_candidate(first_hash, &cumulative);

        cumulative.insert("xt-2".to_string());
        tracker.record_candidate(second_hash, &cumulative);

        assert_eq!(
            tracker.take_confirmed(second_hash),
            vec!["xt-1".to_string(), "xt-2".to_string()]
        );
        assert!(tracker.take_confirmed(second_hash).is_empty());
        assert_eq!(tracker.take_confirmed(first_hash), vec!["xt-1".to_string()]);
    }
}
