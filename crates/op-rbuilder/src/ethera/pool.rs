use super::types::{SubmitXtRequest, XtOrderKey};
use alloy_consensus::Transaction;
use alloy_eips::Decodable2718;
use alloy_primitives::{Address, Bytes, TxHash};
use op_alloy_consensus::OpTxEnvelope;
use parking_lot::RwLock;
use reth_primitives_traits::SignedTransaction;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use thiserror::Error;
use tracing::info;

#[derive(Debug, Clone)]
pub struct ExecutableXtInstance {
    pub instance_id: String,
    pub transactions: Vec<Bytes>,
    /// Unique senders whose nonces are consumed by this instance (in encounter order).
    pub senders: Vec<Address>,
}

#[derive(Debug, Clone)]
pub(crate) struct XtBlockedReason {
    pub instance_id: String,
    pub sender: Address,
    pub phase: &'static str,
    pub status: &'static str,
    pub tx_index: usize,
    pub entry_nonce: u64,
    pub current_nonce: Option<u64>,
    pub tx_hash: TxHash,
    pub reason: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum XtReservationPhase {
    PutInbox,
    Xt,
}

impl XtReservationPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::PutInbox => "put_inbox",
            Self::Xt => "xt",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XtEntryStatus {
    Locked,
    Released,
    Included,
}

impl XtEntryStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Locked => "locked",
            Self::Released => "released",
            Self::Included => "included",
        }
    }
}

impl XtEntryStatus {
    fn counts_for_pending(self) -> bool {
        matches!(self, Self::Locked | Self::Released | Self::Included)
    }

    fn reserves_nonce(self) -> bool {
        matches!(self, Self::Locked | Self::Released | Self::Included)
    }

    fn blocks_pool_tx(self) -> bool {
        matches!(self, Self::Locked | Self::Released)
    }
}

#[derive(Debug, Clone)]
struct XtReservation {
    instance_id: String,
    order: XtOrderKey,
    phase: XtReservationPhase,
    tx_index: usize,
    sender: Address,
    nonce: u64,
    tx_hash: TxHash,
    raw: Bytes,
    status: XtEntryStatus,
}

#[derive(Debug, Clone)]
struct XtInstance {
    order: XtOrderKey,
    entries: Vec<XtReservation>,
    senders: Vec<Address>,
}

#[derive(Debug, Clone)]
struct SenderNonceReservation {
    instance_id: String,
    tx_hash: TxHash,
    status: XtEntryStatus,
}

#[derive(Debug, Default)]
struct SenderReservations {
    by_nonce: BTreeMap<u64, SenderNonceReservation>,
    blocking_count: usize,
}

#[derive(Debug, Default)]
struct XtPoolState {
    by_instance: HashMap<String, XtInstance>,
    ordered_instances: BTreeSet<(XtOrderKey, String)>,
    by_sender: HashMap<Address, SenderReservations>,
    /// Chain-wide pool inclusion gate. Holds the IDs of in-flight XT instances
    /// from the moment `submit_locked` arrives until the builder either
    /// physically executes the instance (`note_executed`) or aborts it. While
    /// non-empty, mempool txs from senders not participating in any in-flight
    /// instance are skipped during flashblock build, keeping the executor's
    /// pre-state aligned with the sidecar's simulation baseline. A set rather
    /// than a counter so the gate is resilient to duplicate submit/abort and
    /// to retried `note_executed` calls.
    pool_gate: HashSet<String>,
}

impl XtPoolState {
    fn insert_instance(
        &mut self,
        instance_id: String,
        order: XtOrderKey,
        entries: Vec<XtReservation>,
    ) {
        let senders = unique_senders(&entries);
        self.ordered_instances.insert((order, instance_id.clone()));
        self.by_instance.insert(
            instance_id,
            XtInstance {
                order,
                entries,
                senders,
            },
        );
    }

    fn remove_instance(&mut self, instance_id: &str) -> Option<XtInstance> {
        let instance = self.by_instance.remove(instance_id)?;
        self.ordered_instances
            .remove(&(instance.order, instance_id.to_string()));
        Some(instance)
    }

    fn rebuild_sender(&mut self, sender: Address) {
        let mut reservations = SenderReservations::default();

        for instance in self.by_instance.values() {
            for entry in &instance.entries {
                if entry.sender != sender || !entry.status.counts_for_pending() {
                    continue;
                }

                let previous = reservations.by_nonce.insert(
                    entry.nonce,
                    SenderNonceReservation {
                        instance_id: entry.instance_id.clone(),
                        tx_hash: entry.tx_hash,
                        status: entry.status,
                    },
                );
                debug_assert!(
                    previous
                        .as_ref()
                        .is_none_or(|existing| existing.tx_hash == entry.tx_hash),
                    "multiple active XT reservations share the same sender nonce"
                );
            }
        }

        reservations.blocking_count = reservations
            .by_nonce
            .values()
            .filter(|reservation| reservation.status.blocks_pool_tx())
            .count();

        if reservations.by_nonce.is_empty() {
            self.by_sender.remove(&sender);
        } else {
            self.by_sender.insert(sender, reservations);
        }
    }

    fn rebuild_senders<I>(&mut self, senders: I)
    where
        I: IntoIterator<Item = Address>,
    {
        let unique: BTreeSet<_> = senders.into_iter().collect();
        for sender in unique {
            self.rebuild_sender(sender);
        }
    }
}

#[derive(Debug, Default)]
pub struct XtPool {
    inner: RwLock<XtPoolState>,
}

#[derive(Debug, Error)]
pub enum XtPoolError {
    #[error("invalid XT transaction: {0}")]
    InvalidTransaction(String),
    #[error("unknown instance {0}")]
    UnknownInstance(String),
    #[error("instance {instance_id} conflicts with reserved nonce {sender:?}:{nonce}")]
    NonceConflict {
        instance_id: String,
        sender: Address,
        nonce: u64,
    },
    #[error("instance {0} already exists with different transactions")]
    InstanceMismatch(String),
}

impl XtPool {
    pub fn submit_locked(&self, request: SubmitXtRequest) -> Result<(), XtPoolError> {
        let reservations = decode_reservations(
            &request.instance_id,
            request.order,
            request.transactions,
            XtReservationPhase::Xt,
            XtEntryStatus::Locked,
        )?;

        let mut state = self.inner.write();
        if let Some(existing) = state.by_instance.get(&request.instance_id) {
            let existing_xt = existing
                .entries
                .iter()
                .filter(|entry| entry.phase == XtReservationPhase::Xt)
                .cloned()
                .collect::<Vec<_>>();
            if same_reservations(&existing_xt, &reservations) {
                // Idempotent re-submit: do not re-close the gate. If
                // `note_executed` has already lifted it, the original
                // submit-to-execute window is what mattered.
                return Ok(());
            }
            return Err(XtPoolError::InstanceMismatch(request.instance_id));
        }

        for reservation in &reservations {
            if conflicts_with_pending(&state, &request.instance_id, reservation) {
                return Err(XtPoolError::NonceConflict {
                    instance_id: reservation.instance_id.clone(),
                    sender: reservation.sender,
                    nonce: reservation.nonce,
                });
            }
        }

        let senders = unique_senders(&reservations);
        // Close the pool gate atomically with the instance becoming visible.
        // Lifted by `note_executed` after the XT runs in a flashblock, or by
        // `abort` if the round is decided to abort.
        state.pool_gate.insert(request.instance_id.clone());
        state.insert_instance(request.instance_id, request.order, reservations);
        state.rebuild_senders(senders);
        Ok(())
    }

    pub fn release(&self, request: super::ReleaseXtRequest) -> Result<(), XtPoolError> {
        let mut state = self.inner.write();
        let (order, existing_put_inbox) = {
            let existing = state
                .by_instance
                .get(&request.instance_id)
                .ok_or_else(|| XtPoolError::UnknownInstance(request.instance_id.clone()))?;
            let order = existing.order;
            let existing_put_inbox = existing
                .entries
                .iter()
                .filter(|entry| entry.phase == XtReservationPhase::PutInbox)
                .cloned()
                .collect::<Vec<_>>();
            (order, existing_put_inbox)
        };

        let released_put_inbox = decode_reservations(
            &request.instance_id,
            order,
            request.transactions,
            XtReservationPhase::PutInbox,
            XtEntryStatus::Released,
        )?;

        if existing_put_inbox.is_empty() {
            for reservation in &released_put_inbox {
                if conflicts_with_pending(&state, &request.instance_id, reservation) {
                    return Err(XtPoolError::NonceConflict {
                        instance_id: reservation.instance_id.clone(),
                        sender: reservation.sender,
                        nonce: reservation.nonce,
                    });
                }
            }
        } else if !same_reservations(&existing_put_inbox, &released_put_inbox) {
            return Err(XtPoolError::InstanceMismatch(request.instance_id));
        }

        let entries = state
            .by_instance
            .get_mut(&request.instance_id)
            .ok_or_else(|| XtPoolError::UnknownInstance(request.instance_id.clone()))?;
        if existing_put_inbox.is_empty() {
            let mut updated_entries = released_put_inbox;
            updated_entries.append(&mut entries.entries);
            entries.entries = updated_entries;
        }
        for entry in &mut entries.entries {
            if entry.phase == XtReservationPhase::Xt && entry.status == XtEntryStatus::Locked {
                entry.status = XtEntryStatus::Released;
            }
        }
        entries.senders = unique_senders(&entries.entries);
        let released_xt_count = entries
            .entries
            .iter()
            .filter(|entry| {
                entry.phase == XtReservationPhase::Xt && entry.status == XtEntryStatus::Released
            })
            .count();
        let released_put_inbox_count = entries
            .entries
            .iter()
            .filter(|entry| {
                entry.phase == XtReservationPhase::PutInbox
                    && entry.status == XtEntryStatus::Released
            })
            .count();
        let senders = entries.senders.clone();
        info!(
            target: "ethera_xt",
            instance_id = %request.instance_id,
            senders = entries.senders.len(),
            released_xt_count,
            released_put_inbox_count,
            "Released XT reservations into executable state"
        );
        state.rebuild_senders(senders);
        Ok(())
    }

    pub fn abort(&self, instance_id: &str) {
        let mut state = self.inner.write();
        // Always clear the gate, even if the instance was never submitted, so
        // an abort racing the submit cannot strand the gate closed.
        state.pool_gate.remove(instance_id);
        let Some(instance) = state.remove_instance(instance_id) else {
            return;
        };
        state.rebuild_senders(instance.senders);
    }

    /// Lift the pool gate for `instance_id` once the builder has physically
    /// executed it inside a flashblock. Called from the flashblock build path
    /// after `execute_xt_transactions` succeeds. Independent of
    /// `mark_included` (which tracks canonical confirmation) so that reorgs
    /// of nonce-reservation status do not have to coordinate with the gate.
    pub fn note_executed(&self, instance_id: &str) {
        let mut state = self.inner.write();
        state.pool_gate.remove(instance_id);
    }

    /// Re-close the pool gate for instances that executed in a candidate
    /// flashblock that was later abandoned before publication. This restores
    /// the submit-to-execute lock without resurrecting already aborted
    /// instances.
    pub fn restore_gate(&self, instance_ids: &[String]) {
        if instance_ids.is_empty() {
            return;
        }

        let mut state = self.inner.write();
        for instance_id in instance_ids {
            if state.by_instance.contains_key(instance_id) {
                state.pool_gate.insert(instance_id.clone());
            }
        }
    }

    /// Returns true while at least one in-flight XT is holding the pool gate
    /// closed. While true, mempool txs from senders without an in-flight XT
    /// reservation are skipped during flashblock build.
    pub fn pool_gate_closed(&self) -> bool {
        !self.inner.read().pool_gate.is_empty()
    }

    pub fn mark_included(&self, instance_ids: &[String]) {
        if instance_ids.is_empty() {
            return;
        }

        let mut state = self.inner.write();
        let mut touched_senders = BTreeSet::new();
        for instance_id in instance_ids {
            let Some(instance) = state.by_instance.get_mut(instance_id) else {
                continue;
            };

            for entry in &mut instance.entries {
                if entry.status == XtEntryStatus::Released {
                    entry.status = XtEntryStatus::Included;
                }
            }
            touched_senders.extend(instance.senders.iter().copied());
        }
        state.rebuild_senders(touched_senders);
    }

    pub fn active_senders(&self) -> Vec<Address> {
        let mut senders = self
            .inner
            .read()
            .by_sender
            .iter()
            .filter(|(_, reservations)| reservations.blocking_count > 0)
            .map(|(sender, _)| *sender)
            .collect::<Vec<_>>();
        senders.sort();
        senders
    }

    pub fn has_reserved_nonce(&self, sender: Address, nonce: u64) -> bool {
        self.inner
            .read()
            .by_sender
            .get(&sender)
            .and_then(|reservations| reservations.by_nonce.get(&nonce))
            .is_some_and(|reservation| reservation.status.reserves_nonce())
    }

    /// Returns true while `sender` has at least one in-flight XT reservation
    /// that blocks pool inclusion at some nonce. Used by the flashblock pool
    /// iterator to allow pool txs from such senders even when the pool gate
    /// is closed, so an XT can wait for a predecessor pool tx to advance the
    /// sender's on-chain nonce. Per-nonce conflicts are still rejected by
    /// `has_blocking_nonce`.
    pub fn is_active_sender(&self, sender: Address) -> bool {
        self.inner
            .read()
            .by_sender
            .get(&sender)
            .is_some_and(|reservations| reservations.blocking_count > 0)
    }

    /// Returns true if the pool gate is closed and this sender is unrelated
    /// to every in-flight XT reservation. The flashblock pool iterator skips
    /// such txs without marking them invalid so they can be reconsidered as
    /// soon as the gate lifts.
    pub fn should_hold_pool_tx(&self, sender: Address) -> bool {
        let state = self.inner.read();
        !state.pool_gate.is_empty()
            && !state
                .by_sender
                .get(&sender)
                .is_some_and(|reservations| reservations.blocking_count > 0)
    }

    pub fn has_blocking_nonce(&self, sender: Address, nonce: u64) -> bool {
        self.inner
            .read()
            .by_sender
            .get(&sender)
            .and_then(|reservations| reservations.by_nonce.get(&nonce))
            .is_some_and(|reservation| reservation.status.blocks_pool_tx())
    }

    pub fn projected_next_nonce(&self, sender: Address, start_nonce: u64) -> u64 {
        let mut next_nonce = start_nonce;
        let state = self.inner.read();
        let Some(reservations) = state.by_sender.get(&sender) else {
            return next_nonce;
        };

        for (&nonce, reservation) in reservations.by_nonce.range(next_nonce..) {
            if !reservation.status.counts_for_pending() {
                continue;
            }
            if nonce > next_nonce {
                break;
            }
            next_nonce = next_nonce.saturating_add(1);
        }

        next_nonce
    }

    pub fn collect_executable_instances(
        &self,
        current_nonces: &HashMap<Address, u64>,
    ) -> Vec<ExecutableXtInstance> {
        let state = self.inner.read();
        let mut expected = current_nonces.clone();
        let mut executable = Vec::new();

        for (_, instance_id) in &state.ordered_instances {
            let Some(instance) = state.by_instance.get(instance_id) else {
                continue;
            };

            let mut instance_expected = expected.clone();
            let mut transactions = Vec::with_capacity(instance.entries.len());
            let mut executable_instance = true;

            for entry in &instance.entries {
                let Some(current_nonce) = instance_expected.get_mut(&entry.sender) else {
                    executable_instance = false;
                    break;
                };

                if entry.nonce < *current_nonce {
                    continue;
                }

                if entry.status != XtEntryStatus::Released || entry.nonce != *current_nonce {
                    executable_instance = false;
                    break;
                }

                transactions.push(entry.raw.clone());
                *current_nonce = current_nonce.saturating_add(1);
            }

            if !executable_instance || transactions.is_empty() {
                continue;
            }

            expected = instance_expected;

            executable.push(ExecutableXtInstance {
                instance_id: instance_id.clone(),
                transactions,
                senders: instance.senders.clone(),
            });
        }

        executable
    }

    pub(crate) fn first_blocked_instance_reason(
        &self,
        current_nonces: &HashMap<Address, u64>,
    ) -> Option<XtBlockedReason> {
        let state = self.inner.read();
        let mut expected = current_nonces.clone();

        for (_, instance_id) in &state.ordered_instances {
            let instance = state.by_instance.get(instance_id)?;
            let mut instance_expected = expected.clone();
            let mut transactions = 0usize;

            for entry in &instance.entries {
                let Some(current_nonce) = instance_expected.get_mut(&entry.sender) else {
                    return Some(XtBlockedReason {
                        instance_id: instance_id.clone(),
                        sender: entry.sender,
                        phase: entry.phase.as_str(),
                        status: entry.status.as_str(),
                        tx_index: entry.tx_index,
                        entry_nonce: entry.nonce,
                        current_nonce: None,
                        tx_hash: entry.tx_hash,
                        reason: "sender nonce unavailable in current state",
                    });
                };

                if entry.nonce < *current_nonce {
                    continue;
                }

                if entry.status != XtEntryStatus::Released {
                    return Some(XtBlockedReason {
                        instance_id: instance_id.clone(),
                        sender: entry.sender,
                        phase: entry.phase.as_str(),
                        status: entry.status.as_str(),
                        tx_index: entry.tx_index,
                        entry_nonce: entry.nonce,
                        current_nonce: Some(*current_nonce),
                        tx_hash: entry.tx_hash,
                        reason: "reservation is not released",
                    });
                }

                if entry.nonce != *current_nonce {
                    return Some(XtBlockedReason {
                        instance_id: instance_id.clone(),
                        sender: entry.sender,
                        phase: entry.phase.as_str(),
                        status: entry.status.as_str(),
                        tx_index: entry.tx_index,
                        entry_nonce: entry.nonce,
                        current_nonce: Some(*current_nonce),
                        tx_hash: entry.tx_hash,
                        reason: "nonce does not match current execution cursor",
                    });
                }

                transactions += 1;
                *current_nonce = current_nonce.saturating_add(1);
            }

            if transactions > 0 {
                expected = instance_expected;
            }
        }

        None
    }
}

fn decode_reservations(
    instance_id: &str,
    order: XtOrderKey,
    transactions: Vec<Bytes>,
    phase: XtReservationPhase,
    status: XtEntryStatus,
) -> Result<Vec<XtReservation>, XtPoolError> {
    transactions
        .into_iter()
        .enumerate()
        .map(|(tx_index, raw)| {
            let signed = OpTxEnvelope::decode_2718(&mut raw.as_ref())
                .map_err(|err| XtPoolError::InvalidTransaction(err.to_string()))?;
            let recovered = signed.try_clone_into_recovered().map_err(|_| {
                XtPoolError::InvalidTransaction("signature recovery failed".to_string())
            })?;

            Ok(XtReservation {
                instance_id: instance_id.to_string(),
                order,
                phase,
                tx_index,
                sender: recovered.signer(),
                nonce: recovered.nonce(),
                tx_hash: recovered.tx_hash(),
                raw,
                status,
            })
        })
        .collect()
}

fn conflicts_with_pending(
    state: &XtPoolState,
    instance_id: &str,
    reservation: &XtReservation,
) -> bool {
    state
        .by_sender
        .get(&reservation.sender)
        .and_then(|reservations| reservations.by_nonce.get(&reservation.nonce))
        .is_some_and(|existing| {
            existing.status.counts_for_pending()
                && !(existing.instance_id == instance_id && existing.tx_hash == reservation.tx_hash)
        })
}

fn same_reservations(existing: &[XtReservation], incoming: &[XtReservation]) -> bool {
    existing.len() == incoming.len()
        && existing.iter().zip(incoming).all(|(left, right)| {
            left.tx_hash == right.tx_hash
                && left.sender == right.sender
                && left.nonce == right.nonce
                && left.order == right.order
                && left.phase == right.phase
                && left.tx_index == right.tx_index
        })
}

fn unique_senders(entries: &[XtReservation]) -> Vec<Address> {
    let mut seen = BTreeSet::new();
    let mut senders = Vec::new();

    for entry in entries {
        if seen.insert(entry.sender) {
            senders.push(entry.sender);
        }
    }

    senders
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ethera::{ReleaseXtRequest, SubmitXtRequest};
    use alloy_eips::Encodable2718;
    use alloy_network::{EthereumWallet, TransactionBuilder};
    use alloy_primitives::Address;
    use alloy_rpc_types_eth::TransactionRequest;
    use alloy_signer_local::PrivateKeySigner;

    const TEST_CHAIN_ID: u64 = 77777;

    async fn signed_tx(signer: &PrivateKeySigner, nonce: u64, to: Address) -> Bytes {
        let wallet = EthereumWallet::new(signer.clone());
        let tx = TransactionRequest::default()
            .with_from(signer.address())
            .with_to(to)
            .with_chain_id(TEST_CHAIN_ID)
            .with_nonce(nonce)
            .gas_limit(21_000)
            .max_priority_fee_per_gas(1_000_000_000)
            .max_fee_per_gas(20_000_000_000);
        let signed = tx.build(&wallet).await.unwrap();
        Bytes::from(signed.encoded_2718())
    }

    #[tokio::test]
    async fn release_executes_put_inbox_before_xt_transactions() {
        let pool = XtPool::default();
        let user = PrivateKeySigner::random();
        let coordinator = PrivateKeySigner::random();
        let xt_tx = signed_tx(&user, 0, Address::repeat_byte(0x11)).await;
        let put_inbox_tx = signed_tx(&coordinator, 5, Address::repeat_byte(0x22)).await;

        pool.submit_locked(SubmitXtRequest {
            instance_id: "xt-1".to_string(),
            order: XtOrderKey {
                period_id: 1,
                sequence_number: 1,
            },
            transactions: vec![xt_tx.clone()],
        })
        .unwrap();
        pool.release(ReleaseXtRequest {
            instance_id: "xt-1".to_string(),
            transactions: vec![put_inbox_tx.clone()],
        })
        .unwrap();

        let executable = pool.collect_executable_instances(&HashMap::from([
            (coordinator.address(), 5),
            (user.address(), 0),
        ]));
        assert_eq!(executable.len(), 1);
        assert_eq!(executable[0].transactions, vec![put_inbox_tx, xt_tx]);
    }

    #[tokio::test]
    async fn release_is_idempotent_for_same_put_inbox_transactions() {
        let pool = XtPool::default();
        let user = PrivateKeySigner::random();
        let coordinator = PrivateKeySigner::random();
        let xt_tx = signed_tx(&user, 0, Address::repeat_byte(0x33)).await;
        let put_inbox_tx = signed_tx(&coordinator, 7, Address::repeat_byte(0x44)).await;

        pool.submit_locked(SubmitXtRequest {
            instance_id: "xt-2".to_string(),
            order: XtOrderKey {
                period_id: 1,
                sequence_number: 2,
            },
            transactions: vec![xt_tx.clone()],
        })
        .unwrap();

        let release = ReleaseXtRequest {
            instance_id: "xt-2".to_string(),
            transactions: vec![put_inbox_tx.clone()],
        };
        pool.release(release.clone()).unwrap();
        pool.release(release).unwrap();

        let executable = pool.collect_executable_instances(&HashMap::from([
            (coordinator.address(), 7),
            (user.address(), 0),
        ]));
        assert_eq!(executable.len(), 1);
        assert_eq!(executable[0].transactions, vec![put_inbox_tx, xt_tx]);
    }

    #[tokio::test]
    async fn included_nonce_stays_reserved_but_stops_blocking_pool_execution() {
        let pool = XtPool::default();
        let user = PrivateKeySigner::random();
        let xt_tx = signed_tx(&user, 0, Address::repeat_byte(0x55)).await;

        pool.submit_locked(SubmitXtRequest {
            instance_id: "xt-3".to_string(),
            order: XtOrderKey {
                period_id: 1,
                sequence_number: 3,
            },
            transactions: vec![xt_tx],
        })
        .unwrap();
        pool.release(ReleaseXtRequest {
            instance_id: "xt-3".to_string(),
            transactions: Vec::new(),
        })
        .unwrap();
        pool.mark_included(&["xt-3".to_string()]);

        assert!(pool.has_reserved_nonce(user.address(), 0));
        assert!(!pool.has_blocking_nonce(user.address(), 0));
    }

    #[tokio::test]
    async fn locked_instance_blocks_later_released_instance_with_same_sender() {
        let pool = XtPool::default();
        let user = PrivateKeySigner::random();
        let xt1 = signed_tx(&user, 0, Address::repeat_byte(0x66)).await;
        let xt2 = signed_tx(&user, 1, Address::repeat_byte(0x77)).await;

        pool.submit_locked(SubmitXtRequest {
            instance_id: "xt-4".to_string(),
            order: XtOrderKey {
                period_id: 1,
                sequence_number: 4,
            },
            transactions: vec![xt1],
        })
        .unwrap();
        pool.submit_locked(SubmitXtRequest {
            instance_id: "xt-5".to_string(),
            order: XtOrderKey {
                period_id: 1,
                sequence_number: 5,
            },
            transactions: vec![xt2],
        })
        .unwrap();
        pool.release(ReleaseXtRequest {
            instance_id: "xt-5".to_string(),
            transactions: Vec::new(),
        })
        .unwrap();

        let executable = pool.collect_executable_instances(&HashMap::from([(user.address(), 0)]));
        assert!(executable.is_empty());
    }

    #[tokio::test]
    async fn pool_gate_closes_on_submit_and_lifts_on_note_executed() {
        let pool = XtPool::default();
        let user = PrivateKeySigner::random();
        let xt_tx = signed_tx(&user, 0, Address::repeat_byte(0x88)).await;

        assert!(!pool.pool_gate_closed());

        pool.submit_locked(SubmitXtRequest {
            instance_id: "gate-1".to_string(),
            order: XtOrderKey {
                period_id: 1,
                sequence_number: 1,
            },
            transactions: vec![xt_tx],
        })
        .unwrap();
        assert!(pool.pool_gate_closed(), "submit_locked must close the gate");

        // Release flips status Locked → Released but the gate stays closed
        // because the executor has not yet run the xT.
        pool.release(ReleaseXtRequest {
            instance_id: "gate-1".to_string(),
            transactions: Vec::new(),
        })
        .unwrap();
        assert!(
            pool.pool_gate_closed(),
            "release alone must not lift the gate; only execution does"
        );

        pool.note_executed("gate-1");
        assert!(
            !pool.pool_gate_closed(),
            "note_executed lifts the gate after physical execution"
        );
    }

    #[tokio::test]
    async fn pool_gate_lifts_on_abort_even_before_submit() {
        let pool = XtPool::default();
        let user = PrivateKeySigner::random();
        let xt_tx = signed_tx(&user, 0, Address::repeat_byte(0x99)).await;

        // Abort racing ahead of submit must not strand the gate closed.
        pool.abort("phantom");
        assert!(!pool.pool_gate_closed());

        pool.submit_locked(SubmitXtRequest {
            instance_id: "gate-2".to_string(),
            order: XtOrderKey {
                period_id: 1,
                sequence_number: 2,
            },
            transactions: vec![xt_tx],
        })
        .unwrap();
        assert!(pool.pool_gate_closed());

        pool.abort("gate-2");
        assert!(!pool.pool_gate_closed(), "abort must lift the gate");
    }

    #[tokio::test]
    async fn pool_gate_stays_closed_until_every_in_flight_instance_clears() {
        let pool = XtPool::default();
        let user_a = PrivateKeySigner::random();
        let user_b = PrivateKeySigner::random();
        let tx_a = signed_tx(&user_a, 0, Address::repeat_byte(0xaa)).await;
        let tx_b = signed_tx(&user_b, 0, Address::repeat_byte(0xbb)).await;

        pool.submit_locked(SubmitXtRequest {
            instance_id: "gate-3a".to_string(),
            order: XtOrderKey {
                period_id: 1,
                sequence_number: 3,
            },
            transactions: vec![tx_a],
        })
        .unwrap();
        pool.submit_locked(SubmitXtRequest {
            instance_id: "gate-3b".to_string(),
            order: XtOrderKey {
                period_id: 1,
                sequence_number: 4,
            },
            transactions: vec![tx_b],
        })
        .unwrap();
        assert!(pool.pool_gate_closed());

        pool.note_executed("gate-3a");
        assert!(
            pool.pool_gate_closed(),
            "gate must remain closed while any in-flight instance is unexecuted"
        );

        pool.note_executed("gate-3b");
        assert!(
            !pool.pool_gate_closed(),
            "gate must lift only after the last in-flight instance executes"
        );
    }

    #[tokio::test]
    async fn duplicate_submit_does_not_reclose_gate_after_execution() {
        let pool = XtPool::default();
        let user = PrivateKeySigner::random();
        let xt_tx = signed_tx(&user, 0, Address::repeat_byte(0xcc)).await;

        let request = SubmitXtRequest {
            instance_id: "gate-4".to_string(),
            order: XtOrderKey {
                period_id: 1,
                sequence_number: 5,
            },
            transactions: vec![xt_tx],
        };

        pool.submit_locked(request.clone()).unwrap();
        pool.note_executed("gate-4");
        assert!(!pool.pool_gate_closed());

        // Idempotent retry of the same submit (e.g. publisher resend) must not
        // re-close the gate: the simulation invariant is bound to the original
        // submit-to-execute window, not to retries.
        pool.submit_locked(request).unwrap();
        assert!(!pool.pool_gate_closed());
    }

    #[tokio::test]
    async fn restore_gate_recloses_only_live_instances() {
        let pool = XtPool::default();
        let user = PrivateKeySigner::random();
        let xt_tx = signed_tx(&user, 0, Address::repeat_byte(0xdd)).await;

        pool.submit_locked(SubmitXtRequest {
            instance_id: "gate-5".to_string(),
            order: XtOrderKey {
                period_id: 1,
                sequence_number: 6,
            },
            transactions: vec![xt_tx],
        })
        .unwrap();
        pool.note_executed("gate-5");
        assert!(!pool.pool_gate_closed());

        pool.restore_gate(&["missing".to_string()]);
        assert!(
            !pool.pool_gate_closed(),
            "abandoned candidates must not resurrect unknown instances"
        );

        pool.restore_gate(&["gate-5".to_string()]);
        assert!(
            pool.pool_gate_closed(),
            "abandoned candidates re-close the gate for live XT reservations"
        );

        pool.abort("gate-5");
        pool.restore_gate(&["gate-5".to_string()]);
        assert!(
            !pool.pool_gate_closed(),
            "aborted instances must not be restored into the gate"
        );
    }
}
