use crate::relay::Relay;
use beacon_api_client::Client;
use futures::future::join_all;
use mev_build_rs::{BlindedBlockProviderServer, EngineBuilder, Network};
use serde::Deserialize;
use std::{
    net::Ipv4Addr, 
    sync::Arc,
    collections::HashMap,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use url::Url;

use rand::seq::SliceRandom;
use httpmock::prelude::*;
use beacon_api_client::{Client as ApiClient, ValidatorStatus, ValidatorSummary, Value};
use ethereum_consensus::{
    bellatrix::mainnet::{BlindedBeaconBlock, BlindedBeaconBlockBody, SignedBlindedBeaconBlock},
    builder::{SignedValidatorRegistration, ValidatorRegistration},
    crypto::SecretKey,
    phase0::mainnet::{compute_domain, Validator},
    primitives::{DomainType, ExecutionAddress, Hash32, Slot},
    signing::sign_with_domain,
    state_transition::Context,
};

use mev_build_rs::{sign_builder_message, BidRequest, BlindedBlockProviderClient as RelayClient};
use mev_boost_rs::{Config as BoostConfig, Service as BoostService};

struct Proposer {
    index: usize,
    validator: Validator,
    signing_key: SecretKey,
    fee_recipient: ExecutionAddress,
}

fn create_proposers<R: rand::Rng>(rng: &mut R, count: usize) -> Vec<Proposer> {
    (0..count)
        .map(|i| {
            let signing_key = SecretKey::random(rng).unwrap();
            let public_key = signing_key.public_key();

            let validator = Validator { public_key, ..Default::default() };

            let fee_recipient = ExecutionAddress::try_from([i as u8; 20].as_ref()).unwrap();

            Proposer { index: i, validator, signing_key, fee_recipient }
        })
        .collect()
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub host: Ipv4Addr,
    pub port: u16,
    pub beacon_node_url: String,
    pub mock: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: Ipv4Addr::UNSPECIFIED,
            port: 28545,
            beacon_node_url: "http://127.0.0.1:5052".into(),
            mock: false,
        }
    }
}

pub struct Service {
    host: Ipv4Addr,
    port: u16,
    beacon_node: Client,
    mock: bool,
    context: Arc<Context>,
}

impl Service {
    pub fn from(config: Config, network: Network) -> Self {
        let endpoint: Url = config.beacon_node_url.parse().unwrap();
        let beacon_node = Client::new(endpoint);
        let context: Context = network.into();
        Self { host: config.host, port: config.port, beacon_node, mock: config.mock, context: Arc::new(context) }
    }

    pub async fn run(&self) {
        let builder = EngineBuilder::new(self.context.clone());
        let relay = Relay::new(builder, self.beacon_node.clone(), self.context.clone());
        relay.initialize().await;

        let block_provider = relay.clone();
        let api_server = BlindedBlockProviderServer::new(self.host, self.port, block_provider);

        let mut tasks = vec![];
        tasks.push(tokio::spawn(async move {
            api_server.run().await;
        }));
        tasks.push(tokio::spawn(async move {
            relay.run().await;
        }));
        join_all(tasks).await;
    }

    pub async fn run_mock(&self) {
        // start mock consensus nodes 
        let mut rng = rand::thread_rng();
        let mut proposers = create_proposers(&mut rng, 4);
        let validator_mock_server = MockServer::start();
        let balance = 32_000_000_000;
        let validators = proposers
            .iter()
            .map(|proposer| ValidatorSummary {
                index: proposer.index,
                balance,
                status: ValidatorStatus::Active,
                validator: Validator {
                    public_key: proposer.signing_key.public_key(),
                    effective_balance: balance,
                    ..Default::default()
                },
            })
            .collect::<Vec<_>>();
        validator_mock_server.mock(|when, then| {
            when.method(GET).path("/eth/v1/beacon/states/head/validators");
            let response =
                serde_json::to_string(&Value { data: validators, meta: HashMap::new() }).unwrap();
            then.status(200).body(response);
        });

        tracing::info!("Mock consensus nodes started");

        // start relay
        let relay_config = Config { 
            host: self.host,
            port: self.port,
            beacon_node_url: validator_mock_server.url(""), // running with mock beacon node
            mock: self.mock,
        }; 
        let relay = Service::from(relay_config, Default::default());
        tokio::spawn(async move { relay.run().await });

        tracing::info!("Mock relay started");

        // start mux (boost) server
        let mut boost_config = BoostConfig::default();
        let relay_port = self.port;
        boost_config.relays.push(format!("http://127.0.0.1:{relay_port}"));

        let mux_port = boost_config.port;
        let service = BoostService::from(boost_config, Default::default());
        tokio::spawn(async move { service.run().await });

        tracing::info!("Mock boost server started");

        // let other tasks run so servers boot before we proceed
        // NOTE: there are more races amongst the various services as we add more
        // hardcode a sleep to ensure services are all booted before
        // proceeding across multiple types of environments
        sleep(Duration::from_secs(1)).await;

        let beacon_node = RelayClient::new(ApiClient::new(
            Url::parse(&format!("http://127.0.0.1:{mux_port}")).unwrap(),
        ));

        beacon_node.check_status().await.unwrap();

        let context = Context::for_mainnet();
        let registrations = proposers
            .iter()
            .map(|proposer| {
                let timestamp = get_time();
                let mut registration = ValidatorRegistration {
                    fee_recipient: proposer.fee_recipient.clone(),
                    gas_limit: 30_000_000,
                    timestamp,
                    public_key: proposer.validator.public_key.clone(),
                };
                let signature =
                    sign_builder_message(&mut registration, &proposer.signing_key, &context).unwrap();
                SignedValidatorRegistration { message: registration, signature }
            })
            .collect::<Vec<_>>();
        beacon_node.register_validators(&registrations).await.unwrap();

        beacon_node.check_status().await.unwrap();

        proposers.shuffle(&mut rng);

        for (i, proposer) in proposers.iter().enumerate() {
            propose_block(&beacon_node, proposer, i, &context).await;
        }

        tracing::info!("Mock Proposers started proposing");
    }
    
    async fn propose_block(
        beacon_node: &RelayClient,
        proposer: &Proposer,
        shuffling_index: usize,
        context: &Context,
    ) {
        let current_slot = 32 + shuffling_index as Slot;
        let parent_hash = Hash32::try_from([shuffling_index as u8; 32].as_ref()).unwrap();
    
        let request = BidRequest {
            slot: current_slot,
            parent_hash: parent_hash.clone(),
            public_key: proposer.validator.public_key.clone(),
        };
        let signed_bid = beacon_node.fetch_best_bid(&request).await.unwrap();
        let bid = &signed_bid.message;
    
        let beacon_block_body = BlindedBeaconBlockBody {
            execution_payload_header: bid.header.clone(),
            ..Default::default()
        };
        let mut beacon_block = BlindedBeaconBlock {
            slot: current_slot,
            proposer_index: proposer.index,
            body: beacon_block_body,
            ..Default::default()
        };

        let domain = compute_domain(DomainType::BeaconProposer, None, None, context).unwrap();
        let signature = sign_with_domain(&mut beacon_block, &proposer.signing_key, domain).unwrap();
        let signed_block = SignedBlindedBeaconBlock { message: beacon_block, signature };
    
        beacon_node.check_status().await.unwrap();
    
        let payload = beacon_node.open_bid(&signed_block).await.unwrap();
    
        beacon_node.check_status().await.unwrap();
    }
    
}
