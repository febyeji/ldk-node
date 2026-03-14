// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::lightning::ln::msgs::SocketAddress;
use reqwest::Client;
use serde_json::Value;

use super::external_node::{ChannelState, ExternalChannel, ExternalNode, TestFailure};

pub(crate) struct TestEclairNode {
	client: Client,
	base_url: String,
	password: String,
	listen_addr: SocketAddress,
}

impl TestEclairNode {
	pub(crate) fn new(base_url: &str, password: &str, listen_addr: SocketAddress) -> Self {
		Self {
			client: Client::new(),
			base_url: base_url.to_string(),
			password: password.to_string(),
			listen_addr,
		}
	}

	pub(crate) fn from_env() -> Self {
		let base_url =
			std::env::var("ECLAIR_API_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
		let password =
			std::env::var("ECLAIR_API_PASSWORD").unwrap_or_else(|_| "eclairpassword".to_string());
		let listen_addr: SocketAddress = std::env::var("ECLAIR_P2P_ADDR")
			.unwrap_or_else(|_| "127.0.0.1:9736".to_string())
			.parse()
			.unwrap();
		Self::new(&base_url, &password, listen_addr)
	}

	async fn post(&self, endpoint: &str, params: &[(&str, &str)]) -> Result<Value, TestFailure> {
		let url = format!("{}{}", self.base_url, endpoint);
		let response = self
			.client
			.post(&url)
			.basic_auth("", Some(&self.password))
			.form(params)
			.send()
			.await
			.map_err(|e| self.make_error(format!("request to {} failed: {}", endpoint, e)))?;

		let status = response.status();
		let body = response
			.text()
			.await
			.map_err(|e| self.make_error(format!("reading response from {}: {}", endpoint, e)))?;

		if !status.is_success() {
			return Err(self.make_error(format!("{} returned {}: {}", endpoint, status, body)));
		}

		serde_json::from_str(&body).map_err(|e| {
			self.make_error(format!("parsing response from {}: {} (body: {})", endpoint, e, body))
		})
	}

	fn make_error(&self, detail: String) -> TestFailure {
		TestFailure::ExternalNodeError { node: "Eclair".to_string(), detail }
	}
}

#[async_trait]
impl ExternalNode for TestEclairNode {
	fn name(&self) -> &str {
		"Eclair"
	}

	async fn get_node_id(&self) -> Result<PublicKey, TestFailure> {
		let info = self.post("/getinfo", &[]).await?;
		let node_id_str = info["nodeId"]
			.as_str()
			.ok_or_else(|| self.make_error("missing nodeId in getinfo response".to_string()))?;
		PublicKey::from_str(node_id_str)
			.map_err(|e| self.make_error(format!("parse nodeId: {}", e)))
	}

	async fn get_listening_address(&self) -> Result<SocketAddress, TestFailure> {
		Ok(self.listen_addr.clone())
	}

	async fn connect_peer(
		&self, peer_id: PublicKey, addr: SocketAddress,
	) -> Result<(), TestFailure> {
		let uri = format!("{}@{}", peer_id, addr);
		self.post("/connect", &[("uri", &uri)]).await?;
		Ok(())
	}

	async fn disconnect_peer(&self, peer_id: PublicKey) -> Result<(), TestFailure> {
		self.post("/disconnect", &[("nodeId", &peer_id.to_string())]).await?;
		Ok(())
	}

	async fn open_channel(
		&self, peer_id: PublicKey, _addr: SocketAddress, capacity_sat: u64, push_msat: Option<u64>,
	) -> Result<String, TestFailure> {
		let mut params =
			vec![("nodeId", peer_id.to_string()), ("fundingSatoshis", capacity_sat.to_string())];
		if let Some(push) = push_msat {
			params.push(("pushMsat", push.to_string()));
		}
		let params_refs: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
		let result = self.post("/open", &params_refs).await?;
		let channel_id = result
			.as_str()
			.ok_or_else(|| self.make_error("open did not return channel id string".to_string()))?;
		Ok(channel_id.to_string())
	}

	async fn close_channel(&self, channel_id: &str) -> Result<(), TestFailure> {
		self.post("/close", &[("channelId", channel_id)]).await?;
		Ok(())
	}

	async fn force_close_channel(&self, channel_id: &str) -> Result<(), TestFailure> {
		self.post("/forceclose", &[("channelId", channel_id)]).await?;
		Ok(())
	}

	async fn create_invoice(
		&self, amount_msat: u64, description: &str,
	) -> Result<String, TestFailure> {
		let amount_str = amount_msat.to_string();
		let result = self
			.post("/createinvoice", &[("amountMsat", &amount_str), ("description", description)])
			.await?;
		let invoice = result["serialized"]
			.as_str()
			.ok_or_else(|| self.make_error("missing serialized in invoice response".to_string()))?;
		Ok(invoice.to_string())
	}

	async fn pay_invoice(&self, invoice: &str) -> Result<String, TestFailure> {
		let result = self.post("/payinvoice", &[("invoice", invoice)]).await?;
		// Eclair returns the payment id
		let payment_id = result.as_str().unwrap_or("").to_string();
		Ok(payment_id)
	}

	async fn send_keysend(
		&self, peer_id: PublicKey, amount_msat: u64,
	) -> Result<String, TestFailure> {
		let amount_str = amount_msat.to_string();
		let node_id_str = peer_id.to_string();
		let result = self
			.post("/sendtonode", &[("nodeId", &node_id_str), ("amountMsat", &amount_str)])
			.await?;
		let payment_id = result.as_str().unwrap_or("").to_string();
		Ok(payment_id)
	}

	async fn get_funding_address(&self) -> Result<String, TestFailure> {
		let result = self.post("/getnewaddress", &[]).await?;
		result
			.as_str()
			.map(|s| s.to_string())
			.ok_or_else(|| self.make_error("getnewaddress did not return string".to_string()))
	}

	async fn list_channels(&self) -> Result<Vec<ExternalChannel>, TestFailure> {
		let result = self.post("/channels", &[]).await?;
		let channels_arr = result
			.as_array()
			.ok_or_else(|| self.make_error("/channels did not return array".to_string()))?;

		let mut channels = Vec::new();
		for ch in channels_arr {
			let channel_id = ch["channelId"].as_str().unwrap_or_default().to_string();
			let node_id_str = ch["nodeId"].as_str().unwrap_or_default();
			let peer_id = match PublicKey::from_str(node_id_str) {
				Ok(pk) => pk,
				Err(_) => continue,
			};
			let state_str = ch["state"].as_str().unwrap_or("");
			let capacity_sat = ch["data"]["commitments"]["active"]
				.as_array()
				.and_then(|a| a.first())
				.and_then(|c| c["fundingTx"]["amountSatoshis"].as_u64())
				.unwrap_or(0);
			let funding_txid = ch["data"]["commitments"]["active"]
				.as_array()
				.and_then(|a| a.first())
				.and_then(|c| c["fundingTx"]["txid"].as_str())
				.map(|s| s.to_string());

			channels.push(ExternalChannel {
				channel_id,
				peer_id,
				capacity_sat,
				funding_txid,
				is_active: state_str == "NORMAL",
			});
		}
		Ok(channels)
	}

	async fn wait_for_channel_state(
		&self, channel_id: &str, target_state: ChannelState, timeout: Duration,
	) -> Result<(), TestFailure> {
		let eclair_state = match target_state {
			ChannelState::Active => "NORMAL",
			ChannelState::PendingOpen => "WAIT_FOR_FUNDING_CONFIRMED",
			ChannelState::PendingClose => "CLOSING",
			ChannelState::Closed => "CLOSED",
		};
		let channel_id_owned = channel_id.to_string();

		tokio::time::timeout(
			timeout,
			super::async_exponential_backoff_poll(|| async {
				let result = self.post("/channels", &[]).await.ok()?;
				let channels_arr = result.as_array()?;
				if eclair_state == "CLOSED" {
					channels_arr
						.iter()
						.all(|ch| ch["channelId"].as_str() != Some(&channel_id_owned))
						.then_some(())
				} else {
					channels_arr
						.iter()
						.find(|ch| {
							ch["channelId"].as_str() == Some(&channel_id_owned)
								&& ch["state"].as_str() == Some(eclair_state)
						})
						.map(|_| ())
				}
			}),
		)
		.await
		.map_err(|_| TestFailure::Timeout {
			operation: format!("wait for channel {} to reach {:?}", channel_id, target_state),
			duration: timeout,
		})
	}
}
