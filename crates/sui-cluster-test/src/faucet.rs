// Copyright (c) 2022, Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
use super::{
    config::{ClusterTestOpt, Env},
    helper::ObjectChecker,
    wallet_client::WalletClient,
    cluster::Cluster,
};
use uuid::Uuid;
use anyhow::bail;
use async_trait::async_trait;
use clap::*;
use sui_types::crypto::KeypairTraits;
use sui::{client_commands::WalletContext, config::SuiClientConfig};
use sui_sdk::{ClientType, crypto::KeystoreType};
use std::collections::HashMap;
use std::sync::Arc;
use sui_faucet::{FaucetResponse, SimpleFaucet, Faucet};
use sui_types::{base_types::{encode_bytes_hex, SuiAddress}, gas_coin::GasCoin, object::Owner};
use tokio::time::{sleep, Duration};
use tracing::{debug, info};
use sui_config::{SUI_KEYSTORE_FILENAME, Config};

const DEVNET_FAUCET_ADDR: &str = "https://faucet.devnet.sui.io:443";
const STAGING_FAUCET_ADDR: &str = "https://faucet.staging.sui.io:443";
const CONTINUOUS_FAUCET_ADDR: &str = "https://faucet.continuous.sui.io:443";


pub struct FaucetClientFactory;

impl FaucetClientFactory {
    #[allow(clippy::borrowed_box)]
    pub async fn new_from_cluster(cluster: &Box<dyn Cluster + Sync + Send>) -> Arc<dyn FaucetClient + Sync + Send> {
        match cluster.remote_faucet_url() {
            Some(url) => Arc::new(RemoteFaucetClient::new(url.into())),
            None => {
                // FIXME refactor
                // If faucet_url is none, it's a local cluster
                let temp_dir = tempfile::tempdir().unwrap();
                let wallet_config_path = temp_dir.path().join("client.yaml");
                let rpc_url = cluster.rpc_url();
                info!("Local faucet use gateway: {}", &rpc_url);
                let keystore_path = temp_dir.path().join(SUI_KEYSTORE_FILENAME);
                let keystore = KeystoreType::File(keystore_path);
                // let key_pair = cluster.user_key();
                let faucet_key = cluster.local_faucet_key().expect("Expect local faucet key for local cluster").copy();
                let address: SuiAddress = faucet_key.public().into();
                keystore.init().unwrap().add_key(faucet_key).unwrap();
                SuiClientConfig {
                    accounts: vec![address],
                    keystore,
                    gateway: ClientType::RPC(rpc_url.into()),
                    active_address: Some(address),
                }
                .persisted(&wallet_config_path)
                .save()
                .unwrap();

                info!(
                    "Faucet initializes wallet from config path: {:?}",
                    wallet_config_path
                );
                let wallet_context = WalletContext::new(&wallet_config_path)
                    .await
                    .unwrap_or_else(|e| {
                        panic!(
                            "Failed to init wallet context from path {:?}, error: {e}",
                            wallet_config_path
                        )
                });
                let simple_faucet = SimpleFaucet::new(wallet_context).await.unwrap();
                Arc::new(LocalFaucetClient{simple_faucet})
            }
        }
    }

    pub fn create(
        options: &ClusterTestOpt,
        wallet_client: &WalletClient,
        // faucet_url: Option<String>,
    ) -> Arc<dyn FaucetClient + Sync + Send> {
        match &options.env {
            Env::NewLocal => {
                let wallet_context = wallet_client.get_wallet();
                Arc::new(DummyFaucetClient::new())
            }
            Env::DevNet => Arc::new(RemoteFaucetClient::new(DEVNET_FAUCET_ADDR.into())),
            Env::Continuous => Arc::new(RemoteFaucetClient::new(CONTINUOUS_FAUCET_ADDR.into())),
            Env::Staging => Arc::new(RemoteFaucetClient::new(STAGING_FAUCET_ADDR.into())),
            Env::CustomRemote => Arc::new(RemoteFaucetClient::new(
                options
                    .faucet_address
                    .clone()
                    .expect("Expect 'faucet_address' for Env::Custom"),
            )),
            // _ => panic!("Unallowed combination of parameters."),
        }
    }
}

/// Faucet Client abstraction
#[async_trait]
pub trait FaucetClient {
    async fn request_sui_coins(
        &self,
        client: &WalletClient,
        minimum_coins: Option<usize>,
    ) -> Result<Vec<GasCoin>, anyhow::Error>;
}

/// Client for a remote faucet that is accessible by POST requests
pub struct RemoteFaucetClient {
    remote_url: String,
}

impl RemoteFaucetClient {
    fn new(url: String) -> Self {
        info!("Use remote faucet: {}", url);
        Self { remote_url: url }
    }
}

#[async_trait]
impl FaucetClient for RemoteFaucetClient {
    /// Request test SUI coins from facuet.
    /// It also verifies the effects are observed by gateway/fullnode.
    async fn request_sui_coins(
        &self,
        client: &WalletClient,
        minimum_coins: Option<usize>,
    ) -> Result<Vec<GasCoin>, anyhow::Error> {
        let gas_url = format!("{}/gas", self.remote_url);
        debug!("Getting coin from remote faucet {}", gas_url);
        let address = client.get_wallet_address();
        let data = HashMap::from([("recipient", encode_bytes_hex(&address))]);
        let map = HashMap::from([("FixedAmountRequest", data)]);

        let response = reqwest::Client::new()
            .post(&gas_url)
            .json(&map)
            .send()
            .await
            .unwrap()
            .json::<FaucetResponse>()
            .await
            .unwrap();

        if let Some(error) = response.error {
            panic!("Failed to get gas tokens with error: {}", error)
        }

        sleep(Duration::from_secs(2)).await;

        // FIXME coinInfo to GasCoin
        let gas_coins = futures::future::join_all(
            response
                .transferred_gas_objects
                .iter()
                .map(|coin_info| {
                    ObjectChecker::new(coin_info.id)
                        .owner(Owner::AddressOwner(address))
                        .check_into_gas_coin(client.get_fullnode())
                })
                .collect::<Vec<_>>(),
        )
        .await
        .into_iter()
        .collect::<Vec<_>>();

        let minimum_coins = minimum_coins.unwrap_or(5);

        if gas_coins.len() < minimum_coins {
            bail!(
                "Expect to get at least {minimum_coins} Sui Coins for address {address}, but only got {}",
                gas_coins.len()
            )
        }

        Ok(gas_coins)
    }
}

/// A dummy faucet that does nothing, suitable for local cluster testing
/// where gas coins are prepared in genesis
pub struct DummyFaucetClient {}

impl DummyFaucetClient {
    fn new() -> Self {
        info!("Use dummy faucet");
        Self {}
    }
}
#[async_trait]
impl FaucetClient for DummyFaucetClient {
    /// Dummy faucet client does not request coins from a real faucet.
    /// Instead it just syncs all gas objects for the address.
    async fn request_sui_coins(
        &self,
        client: &WalletClient,
        minimum_coins: Option<usize>,
    ) -> Result<Vec<GasCoin>, anyhow::Error> {
        let wallet = client.get_wallet();
        let address = client.get_wallet_address();
        client.sync_account_state().await?;
        let gas_coins = wallet
            .gas_objects(address)
            .await?
            .iter()
            .map(|(_amount, o)| GasCoin::try_from(o).unwrap())
            .collect::<Vec<_>>();

        let minimum_coins = minimum_coins.unwrap_or(5);

        if gas_coins.len() < minimum_coins {
            bail!(
                "Expect to get at least {minimum_coins} Sui Coins for address {address}, but only got {}. Try minting more coins on genesis.",
                gas_coins.len()
            )
        }

        Ok(gas_coins)
    }
}


/// A dummy faucet that does nothing, suitable for local cluster testing
/// where gas coins are prepared in genesis
pub struct LocalFaucetClient {
    simple_faucet: SimpleFaucet
}

impl LocalFaucetClient {
    fn new(simple_faucet: SimpleFaucet) -> Self {
        info!("Use local faucet");
        Self {
            simple_faucet,
        }
    }
}
#[async_trait]
impl FaucetClient for LocalFaucetClient {
    async fn request_sui_coins(
        &self,
        client: &WalletClient,
        minimum_coins: Option<usize>,
    ) -> Result<Vec<GasCoin>, anyhow::Error> {

        let wallet = client.get_wallet();
        let address = client.get_wallet_address();
        let receipt = self.simple_faucet.send(Uuid::new_v4(), address, &vec![50000; 5]).await
            .unwrap_or_else(|err| panic!("Failed to get gas tokens with error: {}", err));

        sleep(Duration::from_secs(2)).await;

        // FIXME coinInfo to GasCoin
        let gas_coins = futures::future::join_all(
            receipt
                .sent
                .iter()
                .map(|coin_info| {
                    ObjectChecker::new(coin_info.id)
                        .owner(Owner::AddressOwner(address))
                        .check_into_gas_coin(client.get_fullnode())
                })
                .collect::<Vec<_>>(),
        )
        .await
        .into_iter()
        .collect::<Vec<_>>();

        // client.sync_account_state().await?;
        // let gas_coins = wallet
        //     .gas_objects(address)
        //     .await?
        //     .iter()
        //     .map(|(_amount, o)| GasCoin::try_from(o).unwrap())
        //     .collect::<Vec<_>>();

        let minimum_coins = minimum_coins.unwrap_or(5);

        if gas_coins.len() < minimum_coins {
            bail!(
                "Expect to get at least {minimum_coins} Sui Coins for address {address}, but only got {}. Try minting more coins on genesis.",
                gas_coins.len()
            )
        }


        Ok(gas_coins)
    }
}
