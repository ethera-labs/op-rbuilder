use crate::{
    args::{FlashblocksArgs, OpRbuilderArgs},
    ethera::{ReleaseXtRequest, SubmitXtRequest, XtOrderKey},
    tests::{funded_signer, BlockTransactionsExt, LocalInstance, TransactionBuilderExt},
};
use alloy_consensus::Transaction;
use alloy_eips::Encodable2718;
use alloy_network::TransactionResponse;
use alloy_provider::Provider;
use macros::rb_test;
use std::time::Duration;

#[rb_test(flashblocks, args = OpRbuilderArgs {
    chain_block_time: 1000,
    flashblocks: FlashblocksArgs {
        enabled: true,
        flashblocks_port: 0,
        flashblocks_addr: "127.0.0.1".into(),
        flashblocks_block_time: 200,
        flashblocks_leeway_time: 100,
        flashblocks_fixed: false,
        ..Default::default()
    },
    ..Default::default()
})]
async fn xt_release_unblocks_same_sender_pool_tx_in_same_block(
    rbuilder: LocalInstance,
) -> eyre::Result<()> {
    let driver = rbuilder.driver().await?;
    let user = funded_signer();

    let xt_tx = driver
        .create_transaction()
        .with_signer(user)
        .with_nonce(0)
        .random_valid_transfer()
        .build()
        .await;
    let xt_hash = xt_tx.tx_hash().clone();
    let xt_raw = xt_tx.encoded_2718();

    driver
        .provider()
        .raw_request::<(SubmitXtRequest,), ()>(
            "ethera_submitXt".into(),
            (SubmitXtRequest {
                instance_id: "xt-1".to_string(),
                order: XtOrderKey {
                    period_id: 1,
                    sequence_number: 1,
                },
                transactions: vec![xt_raw.into()],
            },),
        )
        .await?;

    let reserved_nonce = driver
        .provider()
        .get_transaction_count(user.address)
        .pending()
        .await?;
    assert_eq!(reserved_nonce, 1);

    let normal_tx = driver
        .create_transaction()
        .with_signer(user)
        .random_valid_transfer()
        .build()
        .await;
    assert_eq!(normal_tx.nonce(), 1);

    let normal_hash = normal_tx.tx_hash().clone();
    let normal_raw = normal_tx.encoded_2718();
    let _ = driver
        .provider()
        .send_raw_transaction(normal_raw.as_ref())
        .await?;

    let projected_nonce = driver
        .provider()
        .get_transaction_count(user.address)
        .pending()
        .await?;
    assert_eq!(projected_nonce, 2);

    for _ in 0..50 {
        if rbuilder.pool().is_queued(normal_hash) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        rbuilder.pool().is_queued(normal_hash),
        "same-sender tx should remain queued before XT release, got {:?}",
        rbuilder.pool().history(normal_hash)
    );

    driver
        .provider()
        .raw_request::<(ReleaseXtRequest,), ()>(
            "ethera_releaseXt".into(),
            (ReleaseXtRequest {
                instance_id: "xt-1".to_string(),
                transactions: Vec::new(),
            },),
        )
        .await?;

    let block = driver.build_new_block_with_current_timestamp(None).await?;
    assert!(block.includes(&vec![xt_hash, normal_hash]));

    let ordered_hashes = block
        .transactions
        .txns()
        .map(|tx| tx.tx_hash().clone())
        .collect::<Vec<_>>();
    let xt_position = ordered_hashes
        .iter()
        .position(|hash| *hash == xt_hash)
        .expect("XT transaction should be included");
    let normal_position = ordered_hashes
        .iter()
        .position(|hash| *hash == normal_hash)
        .expect("same-sender pool transaction should be included");
    assert!(
        xt_position < normal_position,
        "XT must execute before the queued same-sender tx: {ordered_hashes:?}"
    );

    let latest_nonce = driver
        .provider()
        .get_transaction_count(user.address)
        .await?;
    assert_eq!(latest_nonce, 2);

    Ok(())
}
