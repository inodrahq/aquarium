// Copyright (c) Inodra
// SPDX-License-Identifier: Apache-2.0

//! Minimal blocking GraphQL helper.
//!
//! [`sui_data_store::stores::DataStore`] covers object / transaction / epoch
//! reads, but it does not expose checkpoint-level metadata (latest sequence
//! number, the epoch a checkpoint belongs to). Aquarium needs those to choose
//! and pin a fork point, so this is a tiny direct client for that narrow job.

use anyhow::{Context, Result, anyhow};
use serde_json::json;

/// Mysten's public mainnet GraphQL endpoint (same host `DataStore` reads).
pub const MAINNET_GQL_URL: &str = "https://graphql.mainnet.sui.io/graphql";

/// A narrow GraphQL client for checkpoint metadata.
pub struct Gql {
    client: reqwest::blocking::Client,
    url: String,
}

impl Gql {
    /// Client against the mainnet GraphQL endpoint.
    pub fn mainnet() -> Result<Self> {
        Self::new(MAINNET_GQL_URL)
    }

    /// Client against an explicit GraphQL URL.
    pub fn new(url: &str) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("aquarium/", env!("CARGO_PKG_VERSION")))
            // Bound every call so the CLI can't hang on a stalled connection.
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("building GraphQL client")?;
        Ok(Self {
            client,
            url: url.to_string(),
        })
    }

    fn query(&self, query: &str) -> Result<serde_json::Value> {
        let resp: serde_json::Value = self
            .client
            .post(&self.url)
            .json(&json!({ "query": query }))
            .send()
            .context("GraphQL request failed")?
            // Surface HTTP-level failures (429 rate limit, 5xx) with their status
            // instead of a misleading JSON-decode error on an HTML/text body.
            .error_for_status()
            .context("GraphQL endpoint returned an HTTP error")?
            .json()
            .context("decoding GraphQL response")?;
        if let Some(errors) = resp.get("errors")
            && !errors.is_null()
        {
            return Err(anyhow!("GraphQL errors: {errors}"));
        }
        resp.get("data")
            .cloned()
            .ok_or_else(|| anyhow!("GraphQL response missing `data`: {resp}"))
    }

    /// Sequence number of the latest executed checkpoint on the network.
    pub fn latest_checkpoint(&self) -> Result<u64> {
        let data = self.query("{ checkpoint { sequenceNumber } }")?;
        data["checkpoint"]["sequenceNumber"]
            .as_u64()
            .ok_or_else(|| anyhow!("could not read latest checkpoint sequenceNumber: {data}"))
    }

    /// The epoch a given checkpoint belongs to.
    pub fn checkpoint_epoch(&self, checkpoint: u64) -> Result<u64> {
        let q = format!("{{ checkpoint(sequenceNumber: {checkpoint}) {{ epoch {{ epochId }} }} }}");
        let data = self.query(&q)?;
        data["checkpoint"]["epoch"]["epochId"]
            .as_u64()
            .ok_or_else(|| anyhow!("could not read epoch for checkpoint {checkpoint}: {data}"))
    }

    /// The chain identifier (genesis checkpoint digest), useful as a sanity check.
    pub fn chain_identifier(&self) -> Result<String> {
        let data = self.query("{ chainIdentifier }")?;
        data["chainIdentifier"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("could not read chainIdentifier: {data}"))
    }
}
