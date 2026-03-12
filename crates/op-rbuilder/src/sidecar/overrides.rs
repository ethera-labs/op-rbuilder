//! Helpers for building state overrides for sidecar simulations.

use alloy_primitives::Address;
use reth_revm::State;
use revm::Database;
use serde_json::{Map, Value};
use tracing::warn;

/// Build JSON-RPC state overrides for the current in-progress builder state.
pub fn build_state_overrides<DB: Database>(state: &State<DB>) -> Value {
    let mut overrides = Map::new();

    match state.transition_state.as_ref() {
        Some(transition_state) if !transition_state.transitions.is_empty() => {
            for (addr, account) in transition_state.transitions.iter() {
                let mut state_diff = Map::new();
                for (slot, value) in account.storage.iter() {
                    if value.is_changed() {
                        state_diff.insert(
                            format!("0x{slot:064x}"),
                            Value::String(format!("0x{:064x}", value.present_value())),
                        );
                    }
                }

                let has_account_change =
                    !account.status.is_not_modified() || account.has_new_contract().is_some();
                let has_storage_change = !state_diff.is_empty();
                if !has_account_change && !has_storage_change {
                    continue;
                }

                let mut account_map = Map::new();
                if account.status.was_destroyed() || account.info.is_none() {
                    set_destroyed_account_override(&mut account_map);
                } else if let Some(info) = account.info.as_ref() {
                    set_account_override_fields(&mut account_map, info.balance, info.nonce);
                    if let Some((_hash, code)) = account.has_new_contract() {
                        account_map.insert(
                            "code".to_string(),
                            Value::String(format!("0x{}", hex::encode(code.bytes_slice()))),
                        );
                    }
                }

                if !state_diff.is_empty() && !account_map.contains_key("state") {
                    account_map.insert("stateDiff".to_string(), Value::Object(state_diff));
                }

                if !account_map.is_empty() {
                    overrides.insert(addr.to_string(), Value::Object(account_map));
                }
            }
        }
        Some(_) => {
            if has_cache_modifications(state) {
                warn!(
                    target: "sidecar",
                    "state overrides: empty transition_state while cache has modifications"
                );
            }
        }
        None => {
            if has_cache_modifications(state) {
                warn!(
                    target: "sidecar",
                    "state overrides: transition_state missing while cache has modifications"
                );
            }
        }
    }

    // Cache tracks the builder's current post-execution account state even
    // when transition_state is missing or incomplete. Patch nonce/balance from
    // cache so the sidecar can recognize already-included XTs and avoid
    // redelivering stale transactions in the next flashblock.
    for (addr, account) in state.cache.accounts.iter() {
        if account.status.is_not_modified() {
            continue;
        }

        let account_map = ensure_account_override_entry(&mut overrides, *addr);
        match account.account.as_ref() {
            Some(plain) => {
                set_account_override_fields(account_map, plain.info.balance, plain.info.nonce);
                if let Some(code) = plain.info.code.as_ref() {
                    account_map.entry("code".to_string()).or_insert_with(|| {
                        Value::String(format!("0x{}", hex::encode(code.bytes_slice())))
                    });
                }
            }
            None => set_destroyed_account_override(account_map),
        }
    }

    Value::Object(overrides)
}

fn ensure_account_override_entry<'a>(
    overrides: &'a mut Map<String, Value>,
    addr: Address,
) -> &'a mut Map<String, Value> {
    let entry = overrides
        .entry(addr.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    match entry {
        Value::Object(map) => map,
        _ => unreachable!("state override entry must be a JSON object"),
    }
}

fn set_account_override_fields(
    account_map: &mut Map<String, Value>,
    balance: revm::primitives::U256,
    nonce: u64,
) {
    account_map.insert(
        "balance".to_string(),
        Value::String(format!("0x{:x}", balance)),
    );
    account_map.insert("nonce".to_string(), Value::String(format!("0x{:x}", nonce)));
}

fn set_destroyed_account_override(account_map: &mut Map<String, Value>) {
    account_map.insert("balance".to_string(), Value::String("0x0".to_string()));
    account_map.insert("nonce".to_string(), Value::String("0x0".to_string()));
    account_map.insert("code".to_string(), Value::String("0x".to_string()));
    account_map.insert("state".to_string(), Value::Object(Map::new()));
}

fn has_cache_modifications<DB: Database>(state: &State<DB>) -> bool {
    state
        .cache
        .accounts
        .values()
        .any(|account| !account.status.is_not_modified())
}

#[cfg(test)]
mod tests {
    use super::build_state_overrides;
    use alloy_primitives::Address;
    use reth_revm::State;
    use revm::{database::AccountStatus, state::AccountInfo};
    use serde_json::{Map, Value};

    #[test]
    fn build_state_overrides_uses_cache_nonce_when_transition_state_is_empty() {
        let addr = Address::with_last_byte(0x11);
        let mut state = State::builder().with_bundle_update().build();
        state.insert_account(addr, AccountInfo::default());

        let account = state.cache.accounts.get_mut(&addr).unwrap();
        account.status = AccountStatus::Changed;
        let plain = account.account.as_mut().unwrap();
        plain.info.nonce = 2;
        plain.info.balance = revm::primitives::U256::from(1);

        // Simulate the live failure mode: cache has the updated sender nonce,
        // but transition_state is empty for the next flashblock poll.
        state.transition_state = Some(Default::default());

        let overrides = build_state_overrides(&state);
        let account_override: &Map<String, Value> = overrides
            .as_object()
            .and_then(|map| map.get(&addr.to_string()))
            .and_then(Value::as_object)
            .expect("override for modified account");

        assert_eq!(
            account_override.get("nonce"),
            Some(&Value::String("0x2".to_string()))
        );
        assert_eq!(
            account_override.get("balance"),
            Some(&Value::String("0x1".to_string()))
        );
    }
}
