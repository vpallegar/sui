// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::type_complexity)]

use crate::crypto::{BridgeAuthorityKeyPair, BridgeAuthoritySignInfo};
use crate::error::{BridgeError, BridgeResult};
use crate::eth_client::EthClient;
use crate::sui_client::{SuiClient, SuiClientInner};
use crate::types::{BridgeAction, SignedBridgeAction};
use async_trait::async_trait;
use axum::Json;
use ethers::providers::JsonRpcClient;
use ethers::types::TxHash;
use lru::LruCache;
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::Arc;
use sui_types::digests::TransactionDigest;
use tap::TapFallible;
use tokio::sync::{oneshot, Mutex};
use tracing::info;
use tracing::instrument;

#[async_trait]
pub trait BridgeRequestHandlerTrait {
    /// Handles a request to sign a BridgeAction that bridges assets
    /// from Ethereum to Sui. The inputs are a transaction hash on Ethereum
    /// that emitted the bridge event and the Event index in that transaction
    async fn handle_eth_tx_hash(
        &self,
        tx_hash_hex: String,
        event_idx: u16,
    ) -> Result<Json<SignedBridgeAction>, BridgeError>;
    /// Handles a request to sign a BridgeAction that bridges assets
    /// from Sui to Ethereum. The inputs are a transaction digest on Sui
    /// that emitted the bridge event and the Event index in that transaction
    async fn handle_sui_tx_digest(
        &self,
        tx_digest_base58: String,
        event_idx: u16,
    ) -> Result<Json<SignedBridgeAction>, BridgeError>;
}

#[async_trait::async_trait]
trait ActionVerifier<K>: Send + Sync {
    async fn verify(&self, key: K) -> BridgeResult<BridgeAction>;
}

struct SuiActionVerifier<C> {
    sui_client: Arc<SuiClient<C>>,
}

struct EthActionVerifier<P> {
    eth_client: Arc<EthClient<P>>,
}

#[async_trait::async_trait]
impl<C> ActionVerifier<(TransactionDigest, u16)> for SuiActionVerifier<C>
where
    C: SuiClientInner + Send + Sync + 'static,
{
    async fn verify(&self, key: (TransactionDigest, u16)) -> BridgeResult<BridgeAction> {
        let (tx_digest, event_idx) = key;
        self.sui_client
            .get_bridge_action_by_tx_digest_and_event_idx_maybe(&tx_digest, event_idx)
            .await
            .tap_ok(|action| info!("Sui action found: {:?}", action))
    }
}

#[async_trait::async_trait]
impl<C> ActionVerifier<(TxHash, u16)> for EthActionVerifier<C>
where
    C: JsonRpcClient + Send + Sync + 'static,
{
    async fn verify(&self, key: (TxHash, u16)) -> BridgeResult<BridgeAction> {
        let (tx_hash, event_idx) = key;
        self.eth_client
            .get_finalized_bridge_action_maybe(tx_hash, event_idx)
            .await
            .tap_ok(|action| info!("Eth action found: {:?}", action))
    }
}

struct SignerWithCache<K> {
    signer: Arc<BridgeAuthorityKeyPair>,
    verifier: Arc<dyn ActionVerifier<K>>,
    mutex: Arc<Mutex<()>>,
    cache: LruCache<K, Arc<Mutex<Option<BridgeResult<SignedBridgeAction>>>>>,
}

impl<K> SignerWithCache<K>
where
    K: std::hash::Hash + Eq + Clone + Send + Sync + 'static,
{
    fn new(
        signer: Arc<BridgeAuthorityKeyPair>,
        verifier: impl ActionVerifier<K> + 'static,
    ) -> Self {
        Self {
            signer,
            verifier: Arc::new(verifier),
            mutex: Arc::new(Mutex::new(())),
            cache: LruCache::new(NonZeroUsize::new(1000).unwrap()),
        }
    }

    fn spawn(
        mut self,
        mut rx: mysten_metrics::metered_channel::Receiver<(
            K,
            oneshot::Sender<BridgeResult<SignedBridgeAction>>,
        )>,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let (key, tx) = rx
                    .recv()
                    .await
                    .unwrap_or_else(|| panic!("Server signer's channel is closed"));
                let result = self.sign(key).await;
                // The receiver may be dropped before the sender (client connection was dropped for example),
                // we ignore the error in that case.
                let _ = tx.send(result);
            }
        })
    }

    async fn get_cache_entry(
        &mut self,
        key: K,
    ) -> Arc<Mutex<Option<BridgeResult<SignedBridgeAction>>>> {
        // This mutex exists to make sure everyone gets the same entry, namely no double insert
        let _ = self.mutex.lock().await;
        self.cache
            .get_or_insert(key, || Arc::new(Mutex::new(None)))
            .clone()
    }

    async fn sign(&mut self, key: K) -> BridgeResult<SignedBridgeAction> {
        let signer = self.signer.clone();
        let verifier = self.verifier.clone();
        let entry = self.get_cache_entry(key.clone()).await;
        let mut guard = entry.lock().await;
        if let Some(result) = &*guard {
            return result.clone();
        }
        match verifier.verify(key.clone()).await {
            Ok(bridge_action) => {
                let sig = BridgeAuthoritySignInfo::new(&bridge_action, &signer);
                let result = SignedBridgeAction::new_from_data_and_sig(bridge_action, sig);
                // Cache result if Ok
                *guard = Some(Ok(result.clone()));
                Ok(result)
            }
            Err(e) => {
                match e {
                    // Only cache non-transient errors
                    BridgeError::BridgeEventInUnrecognizedSuiPackage
                    | BridgeError::BridgeEventInUnrecognizedEthContract
                    | BridgeError::BridgeEventNotActionable
                    | BridgeError::NoBridgeEventsInTxPosition => {
                        *guard = Some(Err(e.clone()));
                    }
                    _ => (),
                }
                Err(e)
            }
        }
    }

    #[cfg(test)]
    async fn get_testing_only(
        &mut self,
        key: K,
    ) -> Option<&Arc<Mutex<Option<BridgeResult<SignedBridgeAction>>>>> {
        let _ = self.mutex.lock().await;
        self.cache.get(&key)
    }
}

pub struct BridgeRequestHandler {
    sui_signer_tx: mysten_metrics::metered_channel::Sender<(
        (TransactionDigest, u16),
        oneshot::Sender<BridgeResult<SignedBridgeAction>>,
    )>,
    eth_signer_tx: mysten_metrics::metered_channel::Sender<(
        (TxHash, u16),
        oneshot::Sender<BridgeResult<SignedBridgeAction>>,
    )>,
}

impl BridgeRequestHandler {
    pub fn new<
        SC: SuiClientInner + Send + Sync + 'static,
        EP: JsonRpcClient + Send + Sync + 'static,
    >(
        signer: BridgeAuthorityKeyPair,
        sui_client: Arc<SuiClient<SC>>,
        eth_client: Arc<EthClient<EP>>,
    ) -> Self {
        let (sui_signer_tx, sui_rx) = mysten_metrics::metered_channel::channel(
            1000,
            &mysten_metrics::get_metrics()
                .unwrap()
                .channels
                .with_label_values(&["server_sui_action_signing_queue"]),
        );
        let (eth_signer_tx, eth_rx) = mysten_metrics::metered_channel::channel(
            1000,
            &mysten_metrics::get_metrics()
                .unwrap()
                .channels
                .with_label_values(&["server_eth_action_signing_queue"]),
        );
        let signer = Arc::new(signer);

        SignerWithCache::new(signer.clone(), SuiActionVerifier { sui_client }).spawn(sui_rx);
        SignerWithCache::new(signer.clone(), EthActionVerifier { eth_client }).spawn(eth_rx);

        Self {
            sui_signer_tx,
            eth_signer_tx,
        }
    }
}

#[async_trait]
impl BridgeRequestHandlerTrait for BridgeRequestHandler {
    #[instrument(level = "info", skip(self))]
    async fn handle_eth_tx_hash(
        &self,
        tx_hash_hex: String,
        event_idx: u16,
    ) -> Result<Json<SignedBridgeAction>, BridgeError> {
        info!("Received handle eth tx request");
        let tx_hash = TxHash::from_str(&tx_hash_hex).map_err(|_| BridgeError::InvalidTxHash)?;

        let (tx, rx) = oneshot::channel();
        self.eth_signer_tx
            .send(((tx_hash, event_idx), tx))
            .await
            .unwrap_or_else(|_| panic!("Server eth signing channel is closed"));
        let signed_action = rx
            .blocking_recv()
            .unwrap_or_else(|_| panic!("Server signing task's oneshot channel is dropped"))?;
        Ok(Json(signed_action))
    }

    #[instrument(level = "info", skip(self))]
    async fn handle_sui_tx_digest(
        &self,
        tx_digest_base58: String,
        event_idx: u16,
    ) -> Result<Json<SignedBridgeAction>, BridgeError> {
        info!("Received handle sui tx request");
        let tx_digest = TransactionDigest::from_str(&tx_digest_base58)
            .map_err(|_e| BridgeError::InvalidTxHash)?;
        let (tx, rx) = oneshot::channel();
        self.sui_signer_tx
            .send(((tx_digest, event_idx), tx))
            .await
            .unwrap_or_else(|_| panic!("Server sui signing channel is closed"));
        let signed_action = rx
            .blocking_recv()
            .unwrap_or_else(|_| panic!("Server signing task's oneshot channel is dropped"))?;
        Ok(Json(signed_action))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::{
        eth_mock_provider::EthMockProvider,
        events::{init_all_struct_tags, MoveTokenBridgeEvent, SuiToEthTokenBridgeV1},
        sui_mock_client::SuiMockClient,
        test_utils::{
            get_test_log_and_action, get_test_sui_to_eth_bridge_action, mock_last_finalized_block,
        },
        types::{BridgeActionType, BridgeChainId, TokenId},
    };
    use ethers::types::{Address as EthAddress, TransactionReceipt};
    use sui_json_rpc_types::SuiEvent;
    use sui_types::{base_types::SuiAddress, crypto::get_key_pair};

    #[tokio::test]
    async fn test_sui_signer_with_cache() {
        let (_, kp): (_, BridgeAuthorityKeyPair) = get_key_pair();
        let signer = Arc::new(kp);
        let sui_client_mock = SuiMockClient::default();
        let sui_verifier = SuiActionVerifier {
            sui_client: Arc::new(SuiClient::new_for_testing(sui_client_mock.clone())),
        };
        let mut sui_signer_with_cache = SignerWithCache::new(signer.clone(), sui_verifier);

        // Test `get_cache_entry` creates a new entry if not exist
        let sui_tx_digest = TransactionDigest::random();
        let sui_event_idx = 42;
        assert!(sui_signer_with_cache
            .get_testing_only((sui_tx_digest, sui_event_idx))
            .await
            .is_none());
        let entry = sui_signer_with_cache
            .get_cache_entry((sui_tx_digest, sui_event_idx))
            .await;
        let entry_ = sui_signer_with_cache
            .get_testing_only((sui_tx_digest, sui_event_idx))
            .await;
        assert!(entry_.unwrap().lock().await.is_none());

        let action =
            get_test_sui_to_eth_bridge_action(Some(sui_tx_digest), Some(sui_event_idx), None, None);
        let sig = BridgeAuthoritySignInfo::new(&action, &signer);
        let signed_action = SignedBridgeAction::new_from_data_and_sig(action.clone(), sig);
        entry.lock().await.replace(Ok(signed_action));
        let entry_ = sui_signer_with_cache
            .get_testing_only((sui_tx_digest, sui_event_idx))
            .await;
        assert!(entry_.unwrap().lock().await.is_some());

        // Test `sign` caches Err result
        let sui_tx_digest = TransactionDigest::random();
        let sui_event_idx = 0;

        // Mock an non-cacheable error such as rpc error
        sui_client_mock.add_events_by_tx_digest_error(sui_tx_digest);
        sui_signer_with_cache
            .sign((sui_tx_digest, sui_event_idx))
            .await
            .unwrap_err();
        let entry_ = sui_signer_with_cache
            .get_testing_only((sui_tx_digest, sui_event_idx))
            .await;
        assert!(entry_.unwrap().lock().await.is_none());

        // Mock a cacheable error such as no bridge events in tx position (empty event list)
        sui_client_mock.add_events_by_tx_digest(sui_tx_digest, vec![]);
        assert!(matches!(
            sui_signer_with_cache
                .sign((sui_tx_digest, sui_event_idx))
                .await,
            Err(BridgeError::NoBridgeEventsInTxPosition)
        ));
        let entry_ = sui_signer_with_cache
            .get_testing_only((sui_tx_digest, sui_event_idx))
            .await;
        assert_eq!(
            entry_.unwrap().lock().await.clone().unwrap().unwrap_err(),
            BridgeError::NoBridgeEventsInTxPosition,
        );

        // TODO: test BridgeEventInUnrecognizedSuiPackage, SuiBridgeEvent::try_from_sui_event
        // and BridgeEventNotActionable to be cached

        // Test `sign` caches Ok result
        let emitted_event_1 = MoveTokenBridgeEvent {
            message_type: BridgeActionType::TokenTransfer as u8,
            seq_num: 1,
            source_chain: BridgeChainId::SuiLocalTest as u8,
            sender_address: SuiAddress::random_for_testing_only().to_vec(),
            target_chain: BridgeChainId::EthLocalTest as u8,
            target_address: EthAddress::random().as_bytes().to_vec(),
            token_type: TokenId::USDC as u8,
            amount: 12345,
        };

        // TODO: remove once we don't rely on env var to get package id
        std::env::set_var("BRIDGE_PACKAGE_ID", "0x0b");
        init_all_struct_tags();

        let mut sui_event_1 = SuiEvent::random_for_testing();
        sui_event_1.type_ = SuiToEthTokenBridgeV1.get().unwrap().clone();
        sui_event_1.bcs = bcs::to_bytes(&emitted_event_1).unwrap();
        let sui_tx_digest = sui_event_1.id.tx_digest;

        let mut sui_event_2 = SuiEvent::random_for_testing();
        sui_event_2.type_ = SuiToEthTokenBridgeV1.get().unwrap().clone();
        sui_event_2.bcs = bcs::to_bytes(&emitted_event_1).unwrap();
        let sui_event_idx_2 = 1;
        sui_client_mock.add_events_by_tx_digest(sui_tx_digest, vec![sui_event_2.clone()]);

        sui_client_mock.add_events_by_tx_digest(
            sui_tx_digest,
            vec![sui_event_1.clone(), sui_event_2.clone()],
        );
        let signed_1 = sui_signer_with_cache
            .sign((sui_tx_digest, sui_event_idx))
            .await
            .unwrap();
        let signed_2 = sui_signer_with_cache
            .sign((sui_tx_digest, sui_event_idx_2))
            .await
            .unwrap();

        // Because the result is cached now, the verifier should not be called again.
        // Even though we remove the `add_events_by_tx_digest` mock, we will still get the same result.
        sui_client_mock.add_events_by_tx_digest(sui_tx_digest, vec![]);
        assert_eq!(
            sui_signer_with_cache
                .sign((sui_tx_digest, sui_event_idx))
                .await
                .unwrap(),
            signed_1
        );
        assert_eq!(
            sui_signer_with_cache
                .sign((sui_tx_digest, sui_event_idx_2))
                .await
                .unwrap(),
            signed_2
        );
    }

    #[tokio::test]
    async fn test_eth_signer_with_cache() {
        let (_, kp): (_, BridgeAuthorityKeyPair) = get_key_pair();
        let signer = Arc::new(kp);
        let eth_mock_provider = EthMockProvider::default();
        let contract_address = EthAddress::random();
        let eth_client = EthClient::new_mocked(
            eth_mock_provider.clone(),
            HashSet::from_iter(vec![contract_address]),
        );
        let eth_verifier = EthActionVerifier {
            eth_client: Arc::new(eth_client),
        };
        let mut eth_signer_with_cache = SignerWithCache::new(signer.clone(), eth_verifier);

        // Test `get_cache_entry` creates a new entry if not exist
        let eth_tx_hash = TxHash::random();
        let eth_event_idx = 42;
        assert!(eth_signer_with_cache
            .get_testing_only((eth_tx_hash, eth_event_idx))
            .await
            .is_none());
        let entry = eth_signer_with_cache
            .get_cache_entry((eth_tx_hash, eth_event_idx))
            .await;
        let entry_ = eth_signer_with_cache
            .get_testing_only((eth_tx_hash, eth_event_idx))
            .await;
        // first unwrap should not pacic because the entry should have been inserted by `get_cache_entry`
        assert!(entry_.unwrap().lock().await.is_none());

        let (_, action) = get_test_log_and_action(contract_address, eth_tx_hash, eth_event_idx);
        let sig = BridgeAuthoritySignInfo::new(&action, &signer);
        let signed_action = SignedBridgeAction::new_from_data_and_sig(action.clone(), sig);
        entry.lock().await.replace(Ok(signed_action.clone()));
        let entry_ = eth_signer_with_cache
            .get_testing_only((eth_tx_hash, eth_event_idx))
            .await;
        assert_eq!(
            entry_.unwrap().lock().await.clone().unwrap().unwrap(),
            signed_action
        );

        // Test `sign` caches Ok result
        let eth_tx_hash = TxHash::random();
        let eth_event_idx = 0;
        let (log, _action) = get_test_log_and_action(contract_address, eth_tx_hash, eth_event_idx);
        eth_mock_provider
            .add_response::<[TxHash; 1], TransactionReceipt, TransactionReceipt>(
                "eth_getTransactionReceipt",
                [log.transaction_hash.unwrap()],
                TransactionReceipt {
                    block_number: log.block_number,
                    logs: vec![log.clone()],
                    ..Default::default()
                },
            )
            .unwrap();
        mock_last_finalized_block(&eth_mock_provider, log.block_number.unwrap().as_u64());

        eth_signer_with_cache
            .sign((eth_tx_hash, eth_event_idx))
            .await
            .unwrap();
        let entry_ = eth_signer_with_cache
            .get_testing_only((eth_tx_hash, eth_event_idx))
            .await;
        entry_.unwrap().lock().await.clone().unwrap().unwrap();
    }
}
