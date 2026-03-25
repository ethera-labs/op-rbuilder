use super::types::{SubmitXtRequest, XtOrderKey};
use alloy_consensus::Transaction;
use alloy_eips::Decodable2718;
use alloy_primitives::{Address, Bytes, TxHash};
use op_alloy_consensus::OpTxEnvelope;
use parking_lot::RwLock;
use reth_primitives_traits::SignedTransaction;
use std::collections::{BTreeSet, HashMap};
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct ExecutableXtInstance {
    pub instance_id: String,
    pub transactions: Vec<Bytes>,
    /// Unique senders whose nonces are consumed by this instance (in encounter order).
    pub senders: Vec<Address>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum XtReservationPhase {
    PutInbox,
    Xt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XtEntryStatus {
    Locked,
    Released,
    Included,
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

#[derive(Debug, Default)]
struct XtPoolState {
    by_instance: HashMap<String, Vec<XtReservation>>,
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
                .iter()
                .filter(|entry| entry.phase == XtReservationPhase::Xt)
                .cloned()
                .collect::<Vec<_>>();
            if same_reservations(&existing_xt, &reservations) {
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

        state.by_instance.insert(request.instance_id, reservations);
        Ok(())
    }

    pub fn release(&self, request: super::ReleaseXtRequest) -> Result<(), XtPoolError> {
        let mut state = self.inner.write();
        let (order, existing_put_inbox) = {
            let existing = state
                .by_instance
                .get(&request.instance_id)
                .ok_or_else(|| XtPoolError::UnknownInstance(request.instance_id.clone()))?;
            let order = existing
                .iter()
                .find(|entry| entry.phase == XtReservationPhase::Xt)
                .or_else(|| existing.first())
                .map(|entry| entry.order)
                .ok_or_else(|| XtPoolError::UnknownInstance(request.instance_id.clone()))?;
            let existing_put_inbox = existing
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
            entries.extend(released_put_inbox);
        }
        for entry in entries {
            if entry.phase == XtReservationPhase::Xt && entry.status == XtEntryStatus::Locked {
                entry.status = XtEntryStatus::Released;
            }
        }
        Ok(())
    }

    pub fn abort(&self, instance_id: &str) {
        self.inner.write().by_instance.remove(instance_id);
    }

    pub fn mark_included(&self, instance_ids: &[String]) {
        if instance_ids.is_empty() {
            return;
        }

        let ids: BTreeSet<&str> = instance_ids.iter().map(String::as_str).collect();
        let mut state = self.inner.write();
        for (instance_id, entries) in &mut state.by_instance {
            if !ids.contains(instance_id.as_str()) {
                continue;
            }
            for entry in entries {
                if entry.status == XtEntryStatus::Released {
                    entry.status = XtEntryStatus::Included;
                }
            }
        }
    }

    pub fn prune_confirmed_sender(&self, sender: Address, on_chain_nonce: u64) {
        let mut state = self.inner.write();
        state.by_instance.retain(|_, entries| {
            entries.retain(|entry| !(entry.sender == sender && entry.nonce < on_chain_nonce));
            !entries.is_empty()
        });
    }

    pub fn active_senders(&self) -> Vec<Address> {
        let mut senders = BTreeSet::new();
        for entry in self
            .inner
            .read()
            .by_instance
            .values()
            .flatten()
            .filter(|entry| {
                matches!(
                    entry.status,
                    XtEntryStatus::Locked | XtEntryStatus::Released
                )
            })
        {
            senders.insert(entry.sender);
        }
        senders.into_iter().collect()
    }

    pub fn has_reserved_nonce(&self, sender: Address, nonce: u64) -> bool {
        self.inner
            .read()
            .by_instance
            .values()
            .flatten()
            .any(|entry| entry.sender == sender && entry.nonce == nonce && entry.status.reserves_nonce())
    }

    pub fn has_blocking_nonce(&self, sender: Address, nonce: u64) -> bool {
        self.inner
            .read()
            .by_instance
            .values()
            .flatten()
            .any(|entry| {
                entry.sender == sender && entry.nonce == nonce && entry.status.blocks_pool_tx()
            })
    }

    pub fn projected_next_nonce(&self, sender: Address, start_nonce: u64) -> u64 {
        let mut next_nonce = start_nonce;
        let state = self.inner.read();
        let mut reservations = state
            .by_instance
            .values()
            .flatten()
            .filter(|entry| entry.sender == sender && entry.status.counts_for_pending())
            .collect::<Vec<_>>();
        reservations.sort_by_key(|entry| entry.nonce);

        for entry in reservations {
            if entry.nonce < next_nonce {
                continue;
            }
            if entry.nonce > next_nonce {
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
        let mut entries = self
            .inner
            .read()
            .by_instance
            .values()
            .flatten()
            .filter(|entry| {
                matches!(
                    entry.status,
                    XtEntryStatus::Locked | XtEntryStatus::Released
                )
            })
            .cloned()
            .collect::<Vec<_>>();

        entries.sort_by(|a, b| {
            (a.order, a.instance_id.as_str(), a.phase, a.tx_index).cmp(&(
                b.order,
                b.instance_id.as_str(),
                b.phase,
                b.tx_index,
            ))
        });

        let mut grouped = Vec::<(String, Vec<XtReservation>)>::new();
        for entry in entries {
            match grouped.last_mut() {
                Some((instance_id, instance_entries)) if *instance_id == entry.instance_id => {
                    instance_entries.push(entry);
                }
                _ => grouped.push((entry.instance_id.clone(), vec![entry])),
            }
        }

        let mut expected = current_nonces.clone();
        let mut executable = Vec::new();

        for (instance_id, instance_entries) in grouped {
            let mut instance_expected = expected.clone();
            let mut transactions = Vec::with_capacity(instance_entries.len());
            let mut executable_instance = true;

            for entry in &instance_entries {
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

            let mut senders: Vec<Address> = Vec::new();
            for entry in &instance_entries {
                if !senders.contains(&entry.sender) {
                    senders.push(entry.sender);
                }
            }

            executable.push(ExecutableXtInstance {
                instance_id,
                transactions,
                senders,
            });
        }

        executable
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
        .by_instance
        .iter()
        .flat_map(|(existing_instance_id, entries)| {
            entries
                .iter()
                .filter(move |entry| entry.status.counts_for_pending())
                .map(move |entry| (existing_instance_id.as_str(), entry))
        })
        .any(|(existing_instance_id, entry)| {
            if existing_instance_id == instance_id && entry.tx_hash == reservation.tx_hash {
                return false;
            }

            entry.sender == reservation.sender
                && entry.nonce == reservation.nonce
                && entry.tx_hash != reservation.tx_hash
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
}
