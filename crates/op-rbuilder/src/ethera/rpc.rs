use super::{
    XtPool,
    types::{AbortXtRequest, ReleaseXtRequest, SubmitXtRequest},
};
use crate::sidecar::SidecarClient;
use alloy_consensus::Transaction;
use alloy_eips::{BlockId, Decodable2718};
use alloy_primitives::{Address, B256, Bytes, U256};
use jsonrpsee::{
    core::{RpcResult, async_trait},
    proc_macros::rpc,
};
use op_alloy_consensus::OpTxEnvelope;
use reth::rpc::api::eth::helpers::{EthState, FullEthApi};
use reth_primitives_traits::SignedTransaction;
use reth_rpc_eth_types::{EthApiError, RpcInvalidTransactionError};
use reth_transaction_pool::TransactionPool;
use std::sync::Arc;

#[cfg_attr(not(test), rpc(server, namespace = "ethera"))]
#[cfg_attr(test, rpc(server, client, namespace = "ethera"))]
pub trait EtheraControlApi {
    #[method(name = "submitXt")]
    async fn submit_xt(&self, request: SubmitXtRequest) -> RpcResult<()>;

    #[method(name = "releaseXt")]
    async fn release_xt(&self, request: ReleaseXtRequest) -> RpcResult<()>;

    #[method(name = "abortXt")]
    async fn abort_xt(&self, request: AbortXtRequest) -> RpcResult<()>;
}

#[cfg_attr(not(test), rpc(server, namespace = "eth"))]
#[cfg_attr(test, rpc(server, client, namespace = "eth"))]
pub trait EtheraEthApi {
    #[method(name = "getTransactionCount")]
    async fn get_transaction_count(
        &self,
        address: Address,
        block_id: Option<BlockId>,
    ) -> RpcResult<U256>;

    #[method(name = "sendRawTransaction")]
    async fn send_raw_transaction(&self, tx: Bytes) -> RpcResult<B256>;
}

#[derive(Clone)]
pub struct EtheraRpcExt<Pool, Eth> {
    xt_pool: Arc<XtPool>,
    pool: Pool,
    eth_api: Eth,
    permissions: Option<SidecarClient>,
}

impl<Pool, Eth> EtheraRpcExt<Pool, Eth> {
    pub fn new(
        xt_pool: Arc<XtPool>,
        pool: Pool,
        eth_api: Eth,
        permissions: Option<SidecarClient>,
    ) -> Self {
        Self {
            xt_pool,
            pool,
            eth_api,
            permissions,
        }
    }
}

#[async_trait]
impl<Pool, Eth> EtheraControlApiServer for EtheraRpcExt<Pool, Eth>
where
    Pool: Clone + Send + Sync + 'static,
    Eth: Clone + Send + Sync + 'static,
{
    async fn submit_xt(&self, request: SubmitXtRequest) -> RpcResult<()> {
        self.xt_pool
            .submit_locked(request)
            .map_err(|err| EthApiError::InvalidParams(err.to_string().into()).into())
    }

    async fn release_xt(&self, request: ReleaseXtRequest) -> RpcResult<()> {
        self.xt_pool
            .release(request)
            .map_err(|err| EthApiError::InvalidParams(err.to_string().into()).into())
    }

    async fn abort_xt(&self, request: AbortXtRequest) -> RpcResult<()> {
        self.xt_pool.abort(&request.instance_id);
        Ok(())
    }
}

#[async_trait]
impl<Pool, Eth> EtheraEthApiServer for EtheraRpcExt<Pool, Eth>
where
    Pool: TransactionPool + Clone + Send + Sync + 'static,
    Eth: FullEthApi + Send + Sync + Clone + 'static,
{
    async fn get_transaction_count(
        &self,
        address: Address,
        block_id: Option<BlockId>,
    ) -> RpcResult<U256> {
        if block_id != Some(BlockId::pending()) {
            return EthState::transaction_count(&self.eth_api, address, block_id)
                .await
                .map_err(Into::into);
        }

        let on_chain = EthState::transaction_count(&self.eth_api, address, Some(BlockId::latest()))
            .await
            .map_err(Into::into)?
            .to::<u64>();

        // Alternate between pool and XtPool until neither advances the cursor,
        // handling interleaved reservations (e.g. pool:[0,1], XT:[2,3], pool:[4]).
        let mut next_nonce = on_chain;
        loop {
            let mut advanced = false;

            if let Some(highest_pool_tx) = self
                .pool
                .get_highest_consecutive_transaction_by_sender(address, next_nonce)
            {
                let candidate = highest_pool_tx.nonce().checked_add(1).ok_or_else(|| {
                    EthApiError::InvalidTransaction(RpcInvalidTransactionError::NonceMaxValue)
                })?;
                if candidate > next_nonce {
                    next_nonce = candidate;
                    advanced = true;
                }
            }

            let candidate = self.xt_pool.projected_next_nonce(address, next_nonce);
            if candidate > next_nonce {
                next_nonce = candidate;
                advanced = true;
            }

            if !advanced {
                break;
            }
        }

        Ok(U256::from(next_nonce))
    }

    async fn send_raw_transaction(&self, tx: Bytes) -> RpcResult<B256> {
        let signed = OpTxEnvelope::decode_2718(&mut tx.as_ref())
            .map_err(|err| EthApiError::InvalidParams(err.to_string().into()))?;
        let recovered = signed
            .try_clone_into_recovered()
            .map_err(|_| EthApiError::InvalidParams("signature recovery failed".into()))?;

        if let Some(permissions) = &self.permissions {
            let decision = permissions
                .check_tx(
                    recovered.signer(),
                    recovered.to().is_none(),
                    !recovered.value().is_zero(),
                    signed.tx_hash(),
                )
                .await
                .map_err(|err| {
                    EthApiError::InvalidParams(format!("permission check unavailable: {err}"))
                })?;
            if !decision.allowed {
                let reason = decision.reason.unwrap_or_else(|| "rejected".to_string());
                return Err(
                    EthApiError::InvalidParams(format!("permission denied: {reason}")).into(),
                );
            }
        }

        if self
            .xt_pool
            .has_reserved_nonce(recovered.signer(), recovered.nonce())
        {
            return Err(EthApiError::InvalidParams(
                "nonce reserved by Ethera XT transaction".into(),
            )
            .into());
        }

        self.eth_api
            .send_raw_transaction(tx)
            .await
            .map_err(Into::into)
    }
}
