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

/// `0x2::coin::CoinMetadata` fields for a coin type (from live GraphQL).
#[cfg(feature = "serve")]
pub struct CoinMeta {
    pub decimals: u32,
    pub name: String,
    pub symbol: String,
    pub description: String,
    pub icon_url: String,
    /// Total supply, if the endpoint reports it.
    pub supply: Option<u128>,
}

/// One dynamic field of a parent object (from live GraphQL), flattened to the
/// bits `ListDynamicFields` needs.
#[cfg(feature = "serve")]
pub struct DynField {
    /// Canonical type of the field's name (key).
    pub name_type: String,
    /// BCS bytes of the field's name (key) value.
    pub name_bcs: Vec<u8>,
    /// Canonical type of the field's value (for a dynamic *object* field, the
    /// type of the child object).
    pub value_type: String,
    /// True if this is a dynamic *object* field (value is a child object).
    pub is_object: bool,
    /// The child object's id, for dynamic object fields.
    pub child_id: Option<String>,
}

/// Parse a JSON number-or-string into `u128` (GraphQL returns big ints as
/// strings; some fields as numbers).
#[cfg(feature = "serve")]
fn parse_u128(v: &serde_json::Value) -> Option<u128> {
    v.as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| v.as_u64().map(u128::from))
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

    /// The digest of a given checkpoint (Base58), used to label the fork point.
    pub fn checkpoint_digest(&self, checkpoint: u64) -> Result<String> {
        let q = format!("{{ checkpoint(sequenceNumber: {checkpoint}) {{ digest }} }}");
        let data = self.query(&q)?;
        data["checkpoint"]["digest"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("could not read digest for checkpoint {checkpoint}: {data}"))
    }

    /// `0x2::coin::CoinMetadata` for a coin type on **live** mainnet, if any.
    /// Used to answer `GetCoinInfo` for coin types the fork inherits from
    /// mainnet (fork-created coins are read from the overlay instead).
    #[cfg(feature = "serve")]
    pub fn coin_metadata(&self, coin_type: &str) -> Result<Option<CoinMeta>> {
        let q = format!(
            "{{ coinMetadata(coinType: \"{coin_type}\") {{ decimals name symbol description iconUrl supply }} }}"
        );
        let data = self.query(&q)?;
        let m = &data["coinMetadata"];
        if m.is_null() {
            return Ok(None);
        }
        Ok(Some(CoinMeta {
            decimals: m["decimals"].as_u64().unwrap_or(0) as u32,
            name: m["name"].as_str().unwrap_or_default().to_string(),
            symbol: m["symbol"].as_str().unwrap_or_default().to_string(),
            description: m["description"].as_str().unwrap_or_default().to_string(),
            icon_url: m["iconUrl"].as_str().unwrap_or_default().to_string(),
            supply: parse_u128(&m["supply"]),
        }))
    }

    /// `(canonical coin type, total balance)` across every coin type held by
    /// `owner` on **live** mainnet. Backs `ListBalances` for the mainnet side.
    #[cfg(feature = "serve")]
    pub fn address_balances(&self, owner: &str) -> Result<Vec<(String, u128)>> {
        const PAGE_CAP: usize = 20;
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..PAGE_CAP {
            let after = cursor
                .as_deref()
                .map(|c| format!(", after: \"{c}\""))
                .unwrap_or_default();
            let q = format!(
                "{{ address(address: \"{owner}\") {{ balances(first: 50{after}) {{ pageInfo {{ hasNextPage endCursor }} nodes {{ coinType {{ repr }} totalBalance }} }} }} }}"
            );
            let data = self.query(&q)?;
            let balances = &data["address"]["balances"];
            for node in balances["nodes"].as_array().into_iter().flatten() {
                let coin_type = node["coinType"]["repr"].as_str().unwrap_or_default();
                if coin_type.is_empty() {
                    continue;
                }
                out.push((
                    coin_type.to_string(),
                    parse_u128(&node["totalBalance"]).unwrap_or(0),
                ));
            }
            if balances["pageInfo"]["hasNextPage"].as_bool() != Some(true) {
                return Ok(out);
            }
            cursor = balances["pageInfo"]["endCursor"].as_str().map(String::from);
        }
        Ok(out)
    }

    /// The dynamic fields of `parent` on **live** mainnet (up to `limit`). Backs
    /// `ListDynamicFields` for mainnet parents; locally created fields are read
    /// from the overlay instead.
    #[cfg(feature = "serve")]
    pub fn dynamic_fields(&self, parent: &str, limit: usize) -> Result<Vec<DynField>> {
        use base64::Engine;
        let q = format!(
            "{{ object(address: \"{parent}\") {{ dynamicFields(first: {limit}) {{ nodes {{ \
             name {{ type {{ repr }} bcs }} \
             value {{ __typename ... on MoveValue {{ type {{ repr }} }} ... on MoveObject {{ address contents {{ type {{ repr }} }} }} }} \
             }} }} }} }}"
        );
        let data = self.query(&q)?;
        let mut out = Vec::new();
        for node in data["object"]["dynamicFields"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let name_type = node["name"]["type"]["repr"].as_str().unwrap_or_default();
            let name_bcs = node["name"]["bcs"]
                .as_str()
                .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
                .unwrap_or_default();
            let value = &node["value"];
            let is_object = value["__typename"].as_str() == Some("MoveObject");
            let value_type = if is_object {
                value["contents"]["type"]["repr"].as_str()
            } else {
                value["type"]["repr"].as_str()
            }
            .unwrap_or_default()
            .to_string();
            let child_id = is_object
                .then(|| value["address"].as_str().map(String::from))
                .flatten();
            out.push(DynField {
                name_type: name_type.to_string(),
                name_bcs,
                value_type,
                is_object,
                child_id,
            });
        }
        Ok(out)
    }

    /// The chain identifier (genesis checkpoint digest), useful as a sanity check.
    pub fn chain_identifier(&self) -> Result<String> {
        let data = self.query("{ chainIdentifier }")?;
        data["chainIdentifier"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("could not read chainIdentifier: {data}"))
    }

    /// `(object id, value)` of the `Coin<coin_type>` objects owned by `owner`
    /// on **live** mainnet (GraphQL cannot enumerate by owner at a pinned
    /// checkpoint). Paginates up to a sanity cap; used as the mainnet-side
    /// contribution to fork balance queries.
    pub fn owned_coin_values(&self, owner: &str, coin_type: &str) -> Result<Vec<(String, u64)>> {
        const PAGE_CAP: usize = 8;
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..PAGE_CAP {
            let after = cursor
                .as_deref()
                .map(|c| format!(", after: \"{c}\""))
                .unwrap_or_default();
            let q = format!(
                "{{ address(address: \"{owner}\") {{ objects(filter: {{ type: \"0x2::coin::Coin<{coin_type}>\" }}, first: 50{after}) {{ pageInfo {{ hasNextPage endCursor }} nodes {{ address contents {{ json }} }} }} }} }}"
            );
            let data = self.query(&q)?;
            let objects = &data["address"]["objects"];
            for node in objects["nodes"].as_array().into_iter().flatten() {
                let id = node["address"]
                    .as_str()
                    .ok_or_else(|| anyhow!("coin node missing address: {node}"))?;
                let balance = &node["contents"]["json"]["balance"];
                let value = balance
                    .as_u64()
                    .or_else(|| balance.as_str().and_then(|b| b.parse::<u64>().ok()))
                    .ok_or_else(|| anyhow!("coin node missing balance: {node}"))?;
                out.push((id.to_string(), value));
            }
            if objects["pageInfo"]["hasNextPage"].as_bool() != Some(true) {
                return Ok(out);
            }
            cursor = objects["pageInfo"]["endCursor"].as_str().map(String::from);
        }
        tracing::warn!(
            owner,
            coin_type,
            "owned-coin enumeration truncated at page cap"
        );
        Ok(out)
    }
}
