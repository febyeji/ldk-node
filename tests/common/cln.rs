// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use clightningrpc::lightningrpc::LightningRPC;
use clightningrpc::lightningrpc::PayOptions;
use clightningrpc::requests::AmountOrAll;
use clightningrpc::responses::NetworkAddress;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::lightning::ln::msgs::SocketAddress;
use serde_json::json;

use super::external_node::{ChannelState, ExternalChannel, ExternalNode, TestFailure};

pub(crate) struct TestClnNode {
	client: LightningRPC,
}

impl TestClnNode {
	pub(crate) fn new(socket_path: &str) -> Self {
		Self { client: LightningRPC::new(socket_path) }
	}

	pub(crate) fn from_env() -> Self {
		let sock =
			std::env::var("CLN_SOCKET_PATH").unwrap_or_else(|_| "/tmp/lightning-rpc".to_string());
		Self::new(&sock)
	}

	/// Wait for CLN to sync to chain tip (blockheight > 0).
	pub(crate) async fn wait_for_sync(&self) {
		loop {
			let info = self.client.getinfo().unwrap();
			if info.blockheight > 0 {
				break;
			}
			tokio::time::sleep(Duration::from_millis(250)).await;
		}
	}

	fn make_error(&self, detail: String) -> TestFailure {
		TestFailure::ExternalNodeError { node: "CLN".to_string(), detail }
	}
}

#[async_trait]
impl ExternalNode for TestClnNode {
	fn name(&self) -> &str {
		"CLN"
	}

	async fn get_node_id(&self) -> Result<PublicKey, TestFailure> {
		let info = self.client.getinfo().map_err(|e| self.make_error(format!("getinfo: {}", e)))?;
		PublicKey::from_str(&info.id).map_err(|e| self.make_error(format!("parse node id: {}", e)))
	}

	async fn get_listening_address(&self) -> Result<SocketAddress, TestFailure> {
		let info = self.client.getinfo().map_err(|e| self.make_error(format!("getinfo: {}", e)))?;
		let binding = info
			.binding
			.first()
			.ok_or_else(|| self.make_error("no binding address".to_string()))?;
		match binding {
			NetworkAddress::Ipv4 { address, port } => {
				Ok(std::net::SocketAddrV4::new(*address, *port).into())
			},
			NetworkAddress::Ipv6 { address, port } => {
				Ok(std::net::SocketAddrV6::new(*address, *port, 0, 0).into())
			},
			_ => Err(self.make_error("unsupported CLN address type".to_string())),
		}
	}

	async fn connect_peer(
		&self, peer_id: PublicKey, addr: SocketAddress,
	) -> Result<(), TestFailure> {
		let uri = format!("{}@{}", peer_id, addr);
		let _: serde_json::Value = self
			.client
			.call("connect", &json!({"id": uri}))
			.map_err(|e| self.make_error(format!("connect: {}", e)))?;
		Ok(())
	}

	async fn disconnect_peer(&self, peer_id: PublicKey) -> Result<(), TestFailure> {
		let _: serde_json::Value = self
			.client
			.call("disconnect", &json!({"id": peer_id.to_string()}))
			.map_err(|e| self.make_error(format!("disconnect: {}", e)))?;
		Ok(())
	}

	async fn open_channel(
		&self, peer_id: PublicKey, _addr: SocketAddress, capacity_sat: u64, _push_msat: Option<u64>,
	) -> Result<String, TestFailure> {
		let result = self
			.client
			.fundchannel(&peer_id.to_string(), AmountOrAll::Amount(capacity_sat), None)
			.map_err(|e| self.make_error(format!("fundchannel: {}", e)))?;
		Ok(result.txid)
	}

	async fn close_channel(&self, channel_id: &str) -> Result<(), TestFailure> {
		self.client
			.close(channel_id, None, None)
			.map_err(|e| self.make_error(format!("close: {}", e)))?;
		Ok(())
	}

	async fn force_close_channel(&self, channel_id: &str) -> Result<(), TestFailure> {
		// CLN: close with force=true forces a unilateral close
		self.client
			.close(channel_id, Some(true), None)
			.map_err(|e| self.make_error(format!("force close: {}", e)))?;
		Ok(())
	}

	async fn create_invoice(
		&self, amount_msat: u64, description: &str,
	) -> Result<String, TestFailure> {
		let invoice = self
			.client
			.invoice(Some(amount_msat), description, description, None, None, None)
			.map_err(|e| self.make_error(format!("invoice: {}", e)))?;
		Ok(invoice.bolt11)
	}

	async fn pay_invoice(&self, invoice: &str) -> Result<String, TestFailure> {
		let result = self
			.client
			.pay(invoice, PayOptions::default())
			.map_err(|e| self.make_error(format!("pay: {}", e)))?;
		Ok(result.payment_preimage)
	}

	async fn send_keysend(
		&self, peer_id: PublicKey, amount_msat: u64,
	) -> Result<String, TestFailure> {
		let result: serde_json::Value = self
			.client
			.call("keysend", &json!({"destination": peer_id.to_string(), "msatoshi": amount_msat}))
			.map_err(|e| self.make_error(format!("keysend: {}", e)))?;
		Ok(result["payment_preimage"].as_str().unwrap_or("").to_string())
	}

	async fn get_funding_address(&self) -> Result<String, TestFailure> {
		let addr =
			self.client.newaddr(None).map_err(|e| self.make_error(format!("newaddr: {}", e)))?;
		addr.bech32.ok_or_else(|| self.make_error("no bech32 address returned".to_string()))
	}

	async fn list_channels(&self) -> Result<Vec<ExternalChannel>, TestFailure> {
		let peers = self
			.client
			.listpeers(None, None)
			.map_err(|e| self.make_error(format!("listpeers: {}", e)))?
			.peers;
		let mut channels = Vec::new();
		for peer in peers {
			let peer_id = match PublicKey::from_str(&peer.id) {
				Ok(pk) => pk,
				Err(_) => continue,
			};
			for ch in peer.channels {
				channels.push(ExternalChannel {
					channel_id: ch.short_channel_id.unwrap_or_default(),
					peer_id,
					capacity_sat: ch.total_msat.0 / 1000,
					funding_txid: Some(ch.funding_txid),
					is_active: ch.state == "CHANNELD_NORMAL",
				});
			}
		}
		Ok(channels)
	}

	async fn wait_for_channel_state(
		&self, channel_id: &str, target_state: ChannelState, timeout: Duration,
	) -> Result<(), TestFailure> {
		let cln_state = match target_state {
			ChannelState::Active => "CHANNELD_NORMAL",
			ChannelState::PendingOpen => "CHANNELD_AWAITING_LOCKIN",
			ChannelState::PendingClose => "CHANNELD_SHUTTING_DOWN",
			ChannelState::Closed => "ONCHAIN",
		};
		let channel_id_owned = channel_id.to_string();

		tokio::time::timeout(
			timeout,
			super::async_exponential_backoff_poll(|| async {
				let peers = self.client.listpeers(None, None).ok()?.peers;
				for peer in &peers {
					for ch in &peer.channels {
						if ch.short_channel_id.as_deref() == Some(&channel_id_owned)
							&& ch.state == cln_state
						{
							return Some(());
						}
					}
				}
				None
			}),
		)
		.await
		.map_err(|_| TestFailure::Timeout {
			operation: format!("wait for channel {} to reach {:?}", channel_id, target_state),
			duration: timeout,
		})
	}
}
