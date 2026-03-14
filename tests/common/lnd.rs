// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use bitcoin::hashes::Hash;
use bitcoin::hex::DisplayHex;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::lightning::ln::msgs::SocketAddress;
use lnd_grpc_rust::lnrpc::{
	CloseChannelRequest as LndCloseChannelRequest, ConnectPeerRequest as LndConnectPeerRequest,
	DisconnectPeerRequest as LndDisconnectPeerRequest, GetInfoRequest as LndGetInfoRequest,
	Invoice as LndInvoice, LightningAddress as LndLightningAddress,
	ListChannelsRequest as LndListChannelsRequest, OpenChannelRequest as LndOpenChannelRequest,
	SendRequest as LndSendRequest,
};
use lnd_grpc_rust::{connect, LndClient};
use tokio::fs;
use tokio::sync::Mutex;

use super::external_node::{ChannelState, ExternalChannel, ExternalNode, TestFailure};

pub(crate) struct TestLndNode {
	client: Mutex<LndClient>,
	listen_addr: SocketAddress,
}

impl TestLndNode {
	pub(crate) async fn new(
		cert_path: String, macaroon_path: String, endpoint: String, listen_addr: SocketAddress,
	) -> Self {
		let cert_bytes = fs::read(&cert_path).await.expect("Failed to read TLS cert file");
		let mac_bytes = fs::read(&macaroon_path).await.expect("Failed to read macaroon file");
		let cert = cert_bytes.as_hex().to_string();
		let macaroon = mac_bytes.as_hex().to_string();
		let client = connect(cert, macaroon, endpoint).await.expect("Failed to connect to LND");
		Self { client: Mutex::new(client), listen_addr }
	}

	pub(crate) async fn from_env() -> Self {
		let cert_path = std::env::var("LND_CERT_PATH").expect("LND_CERT_PATH not set");
		let macaroon_path = std::env::var("LND_MACAROON_PATH").expect("LND_MACAROON_PATH not set");
		let endpoint =
			std::env::var("LND_ENDPOINT").unwrap_or_else(|_| "127.0.0.1:8081".to_string());
		let listen_addr: SocketAddress = "127.0.0.1:9735".parse().unwrap();
		Self::new(cert_path, macaroon_path, endpoint, listen_addr).await
	}

	fn make_error(&self, detail: String) -> TestFailure {
		TestFailure::ExternalNodeError { node: "LND".to_string(), detail }
	}
}

#[async_trait]
impl ExternalNode for TestLndNode {
	fn name(&self) -> &str {
		"LND"
	}

	async fn get_node_id(&self) -> Result<PublicKey, TestFailure> {
		let mut client = self.client.lock().await;
		let response = client
			.lightning()
			.get_info(LndGetInfoRequest {})
			.await
			.map_err(|e| self.make_error(format!("get_info: {}", e)))?
			.into_inner();
		PublicKey::from_str(&response.identity_pubkey)
			.map_err(|e| self.make_error(format!("parse pubkey: {}", e)))
	}

	async fn get_listening_address(&self) -> Result<SocketAddress, TestFailure> {
		Ok(self.listen_addr.clone())
	}

	async fn connect_peer(
		&self, peer_id: PublicKey, addr: SocketAddress,
	) -> Result<(), TestFailure> {
		let mut client = self.client.lock().await;
		let request = LndConnectPeerRequest {
			addr: Some(LndLightningAddress { pubkey: peer_id.to_string(), host: addr.to_string() }),
			..Default::default()
		};
		client
			.lightning()
			.connect_peer(request)
			.await
			.map_err(|e| self.make_error(format!("connect_peer: {}", e)))?;
		Ok(())
	}

	async fn disconnect_peer(&self, peer_id: PublicKey) -> Result<(), TestFailure> {
		let mut client = self.client.lock().await;
		let request = LndDisconnectPeerRequest { pub_key: peer_id.to_string() };
		client
			.lightning()
			.disconnect_peer(request)
			.await
			.map_err(|e| self.make_error(format!("disconnect_peer: {}", e)))?;
		Ok(())
	}

	async fn open_channel(
		&self, peer_id: PublicKey, _addr: SocketAddress, capacity_sat: u64, push_msat: Option<u64>,
	) -> Result<String, TestFailure> {
		let mut client = self.client.lock().await;
		let request = LndOpenChannelRequest {
			node_pubkey: peer_id.serialize().to_vec(),
			local_funding_amount: capacity_sat as i64,
			push_sat: push_msat.map(|m| (m / 1000) as i64).unwrap_or(0),
			..Default::default()
		};
		let response = client
			.lightning()
			.open_channel_sync(request)
			.await
			.map_err(|e| self.make_error(format!("open_channel: {}", e)))?
			.into_inner();
		// Construct channel point string from response
		let txid_bytes = match response.funding_txid {
			Some(lnd_grpc_rust::lnrpc::channel_point::FundingTxid::FundingTxidBytes(bytes)) => {
				bytes
			},
			Some(lnd_grpc_rust::lnrpc::channel_point::FundingTxid::FundingTxidStr(s)) => {
				s.into_bytes()
			},
			None => return Err(self.make_error("No funding txid in response".to_string())),
		};
		// LND returns txid bytes in reversed order
		let mut txid_arr = [0u8; 32];
		txid_arr.copy_from_slice(&txid_bytes);
		txid_arr.reverse();
		let txid_hex = txid_arr.as_hex().to_string();
		Ok(format!("{}:{}", txid_hex, response.output_index))
	}

	async fn close_channel(&self, channel_id: &str) -> Result<(), TestFailure> {
		let mut client = self.client.lock().await;
		let (txid_bytes, output_index) = parse_channel_point(channel_id)?;
		let request = LndCloseChannelRequest {
			channel_point: Some(lnd_grpc_rust::lnrpc::ChannelPoint {
				funding_txid: Some(
					lnd_grpc_rust::lnrpc::channel_point::FundingTxid::FundingTxidBytes(txid_bytes),
				),
				output_index,
			}),
			..Default::default()
		};
		client
			.lightning()
			.close_channel(request)
			.await
			.map_err(|e| self.make_error(format!("close_channel: {}", e)))?;
		Ok(())
	}

	async fn force_close_channel(&self, channel_id: &str) -> Result<(), TestFailure> {
		let mut client = self.client.lock().await;
		let (txid_bytes, output_index) = parse_channel_point(channel_id)?;
		let request = LndCloseChannelRequest {
			channel_point: Some(lnd_grpc_rust::lnrpc::ChannelPoint {
				funding_txid: Some(
					lnd_grpc_rust::lnrpc::channel_point::FundingTxid::FundingTxidBytes(txid_bytes),
				),
				output_index,
			}),
			force: true,
			..Default::default()
		};
		client
			.lightning()
			.close_channel(request)
			.await
			.map_err(|e| self.make_error(format!("force_close_channel: {}", e)))?;
		Ok(())
	}

	async fn create_invoice(
		&self, amount_msat: u64, _description: &str,
	) -> Result<String, TestFailure> {
		let mut client = self.client.lock().await;
		let invoice = LndInvoice { value_msat: amount_msat as i64, ..Default::default() };
		let response = client
			.lightning()
			.add_invoice(invoice)
			.await
			.map_err(|e| self.make_error(format!("create_invoice: {}", e)))?
			.into_inner();
		Ok(response.payment_request)
	}

	async fn pay_invoice(&self, invoice: &str) -> Result<String, TestFailure> {
		let mut client = self.client.lock().await;
		let send_req =
			LndSendRequest { payment_request: invoice.to_string(), ..Default::default() };
		let response = client
			.lightning()
			.send_payment_sync(send_req)
			.await
			.map_err(|e| self.make_error(format!("pay_invoice: {}", e)))?
			.into_inner();
		if !response.payment_error.is_empty() {
			return Err(self.make_error(format!("payment failed: {}", response.payment_error)));
		}
		if response.payment_preimage.is_empty() {
			return Err(self.make_error("No preimage returned".to_string()));
		}
		Ok(response.payment_preimage.as_hex().to_string())
	}

	async fn send_keysend(
		&self, peer_id: PublicKey, amount_msat: u64,
	) -> Result<String, TestFailure> {
		let mut client = self.client.lock().await;
		let send_req = LndSendRequest {
			dest: peer_id.serialize().to_vec(),
			amt_msat: amount_msat as i64,
			..Default::default()
		};
		let response = client
			.lightning()
			.send_payment_sync(send_req)
			.await
			.map_err(|e| self.make_error(format!("send_keysend: {}", e)))?
			.into_inner();
		if !response.payment_error.is_empty() {
			return Err(self.make_error(format!("keysend failed: {}", response.payment_error)));
		}
		Ok(response.payment_preimage.as_hex().to_string())
	}

	async fn get_funding_address(&self) -> Result<String, TestFailure> {
		let mut client = self.client.lock().await;
		let response = client
			.lightning()
			.new_address(lnd_grpc_rust::lnrpc::NewAddressRequest {
				r#type: 4, // TAPROOT_PUBKEY
				..Default::default()
			})
			.await
			.map_err(|e| self.make_error(format!("get_funding_address: {}", e)))?
			.into_inner();
		Ok(response.address)
	}

	async fn list_channels(&self) -> Result<Vec<ExternalChannel>, TestFailure> {
		let mut client = self.client.lock().await;
		let response = client
			.lightning()
			.list_channels(LndListChannelsRequest { ..Default::default() })
			.await
			.map_err(|e| self.make_error(format!("list_channels: {}", e)))?
			.into_inner();
		let channels = response
			.channels
			.into_iter()
			.filter_map(|ch| {
				let peer_id = match PublicKey::from_str(&ch.remote_pubkey) {
					Ok(pk) => pk,
					Err(_) => return None,
				};
				Some(ExternalChannel {
					channel_id: ch.channel_point.clone(),
					peer_id,
					capacity_sat: ch.capacity as u64,
					funding_txid: ch.channel_point.split(':').next().map(|s| s.to_string()),
					is_active: ch.active,
				})
			})
			.collect();
		Ok(channels)
	}

	async fn wait_for_channel_state(
		&self, channel_id: &str, target_state: ChannelState, timeout: Duration,
	) -> Result<(), TestFailure> {
		let channel_id_owned = channel_id.to_string();
		tokio::time::timeout(
			timeout,
			super::async_exponential_backoff_poll(|| async {
				let channels = self.list_channels().await.ok()?;
				let found = match target_state {
					ChannelState::Active => {
						channels.iter().any(|ch| ch.channel_id == channel_id_owned && ch.is_active)
					},
					ChannelState::Closed => {
						channels.iter().all(|ch| ch.channel_id != channel_id_owned)
					},
					ChannelState::PendingOpen | ChannelState::PendingClose => {
						channels.iter().any(|ch| ch.channel_id == channel_id_owned && !ch.is_active)
					},
				};
				found.then_some(())
			}),
		)
		.await
		.map_err(|_| TestFailure::Timeout {
			operation: format!("wait for channel {} to reach {:?}", channel_id, target_state),
			duration: timeout,
		})
	}
}

/// Parse a channel point string "txid:output_index" into (txid_bytes, output_index).
fn parse_channel_point(channel_point: &str) -> Result<(Vec<u8>, u32), TestFailure> {
	let parts: Vec<&str> = channel_point.split(':').collect();
	if parts.len() != 2 {
		return Err(TestFailure::ExternalNodeError {
			node: "LND".to_string(),
			detail: format!("Invalid channel point format: {}", channel_point),
		});
	}
	let txid = bitcoin::Txid::from_str(parts[0]).map_err(|e| TestFailure::ExternalNodeError {
		node: "LND".to_string(),
		detail: format!("Invalid txid in channel point: {}", e),
	})?;
	let output_index: u32 = parts[1].parse().map_err(|e| TestFailure::ExternalNodeError {
		node: "LND".to_string(),
		detail: format!("Invalid output index in channel point: {}", e),
	})?;
	Ok((txid.as_byte_array().to_vec(), output_index))
}
