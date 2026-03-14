// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use ldk_node::bitcoin::secp256k1::PublicKey;
use ldk_node::lightning::ln::msgs::SocketAddress;

/// Represents a channel opened to or from an external Lightning node.
#[derive(Debug, Clone)]
pub(crate) struct ExternalChannel {
	pub channel_id: String,
	pub peer_id: PublicKey,
	pub capacity_sat: u64,
	pub funding_txid: Option<String>,
	pub is_active: bool,
}

/// The lifecycle state of a channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChannelState {
	PendingOpen,
	Active,
	PendingClose,
	Closed,
}

/// Errors that can occur during interop test operations.
#[derive(Debug)]
pub(crate) enum TestFailure {
	Timeout { operation: String, duration: Duration },
	UnexpectedState { expected: String, actual: String },
	ExternalNodeError { node: String, detail: String },
}

impl fmt::Display for TestFailure {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			TestFailure::Timeout { operation, duration } => {
				write!(f, "Timeout waiting for '{}' after {:?}", operation, duration)
			},
			TestFailure::UnexpectedState { expected, actual } => {
				write!(f, "Unexpected state: expected '{}', got '{}'", expected, actual)
			},
			TestFailure::ExternalNodeError { node, detail } => {
				write!(f, "External node '{}' error: {}", node, detail)
			},
		}
	}
}

impl std::error::Error for TestFailure {}

/// Abstraction over an external Lightning node used in interop tests.
#[async_trait]
pub(crate) trait ExternalNode: Send + Sync {
	/// Human-readable name for this node (e.g. "eclair", "lnd", "cln").
	fn name(&self) -> &str;

	/// Returns the node's public key.
	async fn get_node_id(&self) -> Result<PublicKey, TestFailure>;

	/// Returns an address on which this node is listening.
	async fn get_listening_address(&self) -> Result<SocketAddress, TestFailure>;

	/// Connect to a peer by public key and address.
	async fn connect_peer(
		&self, peer_id: PublicKey, addr: SocketAddress,
	) -> Result<(), TestFailure>;

	/// Disconnect from a peer by public key.
	async fn disconnect_peer(&self, peer_id: PublicKey) -> Result<(), TestFailure>;

	/// Open a channel to a peer.
	///
	/// Returns a `UserChannelId`-style string that the implementation may use
	/// to correlate with subsequent close/query calls.
	async fn open_channel(
		&self, peer_id: PublicKey, addr: SocketAddress, capacity_sat: u64, push_msat: Option<u64>,
	) -> Result<String, TestFailure>;

	/// Cooperatively close a channel by its implementation-defined channel id.
	async fn close_channel(&self, channel_id: &str) -> Result<(), TestFailure>;

	/// Force-close a channel by its implementation-defined channel id.
	async fn force_close_channel(&self, channel_id: &str) -> Result<(), TestFailure>;

	/// Create a BOLT11 invoice for the given amount.
	async fn create_invoice(
		&self, amount_msat: u64, description: &str,
	) -> Result<String, TestFailure>;

	/// Pay a BOLT11 invoice; returns a payment identifier on success
	/// (preimage for LND/CLN, payment UUID for Eclair).
	async fn pay_invoice(&self, invoice: &str) -> Result<String, TestFailure>;

	/// Send a keysend payment to a peer.
	async fn send_keysend(
		&self, peer_id: PublicKey, amount_msat: u64,
	) -> Result<String, TestFailure>;

	/// Get an on-chain address that can be used to fund this node.
	async fn get_funding_address(&self) -> Result<String, TestFailure>;

	/// List all channels known to this node.
	async fn list_channels(&self) -> Result<Vec<ExternalChannel>, TestFailure>;

	/// Block until a specific channel reaches `target_state` or the timeout elapses.
	async fn wait_for_channel_state(
		&self, channel_id: &str, target_state: ChannelState, timeout: Duration,
	) -> Result<(), TestFailure>;
}
