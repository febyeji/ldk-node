// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Shared interop test scenarios, generic over `ExternalNode`.
//!
//! Each scenario function takes an LDK `Node`, an external node implementation,
//! and the regtest infrastructure clients. Test entry-point files
//! (`integration_tests_{cln,lnd,eclair}.rs`) call these functions with their
//! concrete `ExternalNode` implementation.

use std::str::FromStr;
use std::time::Duration;

use bitcoin::Amount;
use electrsd::corepc_node::Client as BitcoindClient;
use electrum_client::ElectrumApi;
use ldk_node::{Event, Node};
use lightning_invoice::Bolt11Invoice;

use super::external_node::ExternalNode;
use super::{generate_blocks_and_wait, premine_and_distribute_funds};

// ---------------------------------------------------------------------------
// Proptest parameter types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) enum Phase {
	ChannelOpen,
	Payment,
	Close,
	Idle,
}

#[derive(Debug, Clone)]
pub(crate) enum Side {
	Ldk,
	External,
}

#[derive(Debug, Clone)]
pub(crate) enum CloseType {
	Cooperative,
	Force,
}

#[derive(Debug, Clone)]
pub(crate) enum PayType {
	Bolt11,
	Keysend,
}

// ---------------------------------------------------------------------------
// Setup helpers
// ---------------------------------------------------------------------------

/// Fund both LDK node and external node, connect them.
pub(crate) async fn setup_interop_test<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
) {
	// Fund LDK node
	let ldk_address = node.onchain_payment().new_address().unwrap();
	let premine_amount = Amount::from_sat(5_000_000);
	premine_and_distribute_funds(bitcoind, electrs, vec![ldk_address], premine_amount).await;

	// Fund external node using the already-loaded wallet
	let ext_funding_addr_str = peer.get_funding_address().await.unwrap();
	let ext_amount = Amount::from_sat(5_000_000);
	let amounts_json = serde_json::json!({&ext_funding_addr_str: ext_amount.to_btc()});
	let empty_account = serde_json::json!("");
	// Use the ldk_node_test wallet that premine_and_distribute_funds already loaded
	bitcoind
		.call::<serde_json::Value>(
			"sendmany",
			&[empty_account, amounts_json, serde_json::json!(0), serde_json::json!("")],
		)
		.expect("failed to fund external node");
	generate_blocks_and_wait(bitcoind, electrs, 1).await;

	node.sync_wallets().unwrap();

	// Connect LDK to external node
	let ext_node_id = peer.get_node_id().await.unwrap();
	let ext_addr = peer.get_listening_address().await.unwrap();
	node.connect(ext_node_id, ext_addr, true).unwrap();
}

/// Open a channel from LDK to external node, wait for it to be confirmed.
/// Returns (user_channel_id, external_channel_id).
pub(crate) async fn open_channel_to_external<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
	funding_amount_sat: u64, push_msat: Option<u64>,
) -> (ldk_node::UserChannelId, String) {
	let ext_node_id = peer.get_node_id().await.unwrap();
	let ext_addr = peer.get_listening_address().await.unwrap();

	node.open_channel(ext_node_id, ext_addr, funding_amount_sat, push_msat, None).unwrap();

	let funding_txo = expect_channel_pending_event!(node, ext_node_id);
	super::wait_for_tx(electrs, funding_txo.txid).await;
	generate_blocks_and_wait(bitcoind, electrs, 6).await;
	node.sync_wallets().unwrap();
	let user_channel_id = expect_channel_ready_event!(node, ext_node_id);

	// Find the external node's channel ID for this channel
	let ext_channels = peer.list_channels().await.unwrap();
	let funding_txid_str = funding_txo.txid.to_string();
	let ext_channel_id = ext_channels
		.iter()
		.find(|ch| ch.funding_txid.as_deref() == Some(&funding_txid_str))
		.or_else(|| ext_channels.iter().find(|ch| ch.peer_id == node.node_id()))
		.map(|ch| ch.channel_id.clone())
		.unwrap_or_else(|| panic!("Could not find channel on external node {}", peer.name()));

	(user_channel_id, ext_channel_id)
}

// ---------------------------------------------------------------------------
// Disconnect/Reconnect scenarios
// ---------------------------------------------------------------------------

/// Disconnect during idle, reconnect, verify channel still works.
pub(crate) async fn test_disconnect_reconnect_idle<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), _bitcoind: &BitcoindClient, _electrs: &E,
	disconnect_side: &Side,
) {
	let ext_node_id = peer.get_node_id().await.unwrap();
	let ext_addr = peer.get_listening_address().await.unwrap();

	match disconnect_side {
		Side::Ldk => {
			node.disconnect(ext_node_id).unwrap();
		},
		Side::External => {
			peer.disconnect_peer(node.node_id()).await.unwrap();
		},
	}

	tokio::time::sleep(Duration::from_secs(1)).await;

	// Reconnect
	node.connect(ext_node_id, ext_addr, true).unwrap();
	tokio::time::sleep(Duration::from_secs(1)).await;

	// Verify channel still works with a payment
	let invoice_str = peer.create_invoice(10_000_000, "disconnect-idle-test").await.unwrap();
	let parsed_invoice = Bolt11Invoice::from_str(&invoice_str).unwrap();
	node.bolt11_payment().send(&parsed_invoice, None).unwrap();
	expect_event!(node, PaymentSuccessful);
}

/// Disconnect during payment, reconnect, verify payment resolves.
pub(crate) async fn test_disconnect_during_payment<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), _bitcoind: &BitcoindClient, _electrs: &E,
	disconnect_side: &Side,
) {
	let ext_node_id = peer.get_node_id().await.unwrap();
	let ext_addr = peer.get_listening_address().await.unwrap();

	let invoice_str = peer.create_invoice(10_000_000, "disconnect-payment-test").await.unwrap();
	let parsed_invoice = Bolt11Invoice::from_str(&invoice_str).unwrap();

	// Send payment (may or may not complete before disconnect)
	let _ = node.bolt11_payment().send(&parsed_invoice, None);

	// Disconnect immediately
	match disconnect_side {
		Side::Ldk => {
			let _ = node.disconnect(ext_node_id);
		},
		Side::External => {
			let _ = peer.disconnect_peer(node.node_id()).await;
		},
	}

	tokio::time::sleep(Duration::from_secs(2)).await;

	// Reconnect
	node.connect(ext_node_id, ext_addr, true).unwrap();
	tokio::time::sleep(Duration::from_secs(2)).await;

	// Payment should eventually resolve
	let event = node.next_event_async().await;
	match event {
		ldk_node::Event::PaymentSuccessful { .. } | ldk_node::Event::PaymentFailed { .. } => {
			node.event_handled().unwrap();
		},
		other => {
			panic!("Expected payment outcome event, got: {:?}", other);
		},
	}
}

// ---------------------------------------------------------------------------
// Channel Closing scenarios
// ---------------------------------------------------------------------------

/// Cooperative close initiated by LDK.
pub(crate) async fn test_cooperative_close_by_ldk(
	node: &Node, peer: &(impl ExternalNode + ?Sized), user_channel_id: &ldk_node::UserChannelId,
) {
	let ext_node_id = peer.get_node_id().await.unwrap();
	node.close_channel(user_channel_id, ext_node_id).unwrap();
	expect_event!(node, ChannelClosed);
}

/// Cooperative close initiated by external node.
pub(crate) async fn test_cooperative_close_by_external(
	node: &Node, peer: &(impl ExternalNode + ?Sized), ext_channel_id: &str,
) {
	peer.close_channel(ext_channel_id).await.unwrap();
	expect_event!(node, ChannelClosed);
}

/// Force close by LDK.
pub(crate) async fn test_force_close_by_ldk<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
	user_channel_id: &ldk_node::UserChannelId,
) {
	let ext_node_id = peer.get_node_id().await.unwrap();
	node.force_close_channel(user_channel_id, ext_node_id, None).unwrap();
	expect_event!(node, ChannelClosed);
	generate_blocks_and_wait(bitcoind, electrs, 6).await;
	node.sync_wallets().unwrap();
}

/// Force close by external node.
pub(crate) async fn test_force_close_by_external<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
	ext_channel_id: &str,
) {
	peer.force_close_channel(ext_channel_id).await.unwrap();
	generate_blocks_and_wait(bitcoind, electrs, 6).await;
	node.sync_wallets().unwrap();
	expect_event!(node, ChannelClosed);
}

/// Force close with pending HTLCs.
pub(crate) async fn test_force_close_with_pending_htlcs<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
	user_channel_id: &ldk_node::UserChannelId, ext_channel_id: &str, close_side: &Side,
) {
	let ext_node_id = peer.get_node_id().await.unwrap();

	// Send a payment that will be in-flight
	let invoice_str = peer.create_invoice(10_000_000, "pending-htlc-test").await.unwrap();
	let parsed_invoice = Bolt11Invoice::from_str(&invoice_str).unwrap();
	let _ = node.bolt11_payment().send(&parsed_invoice, None);

	// Force close immediately while payment is in-flight
	match close_side {
		Side::Ldk => {
			node.force_close_channel(user_channel_id, ext_node_id, None).unwrap();
		},
		Side::External => {
			peer.force_close_channel(ext_channel_id).await.unwrap();
		},
	}

	// Mine enough blocks for HTLC timeout and sweep
	generate_blocks_and_wait(bitcoind, electrs, 150).await;
	node.sync_wallets().unwrap();

	// Drain events — expect ChannelClosed and possibly PaymentFailed
	loop {
		match tokio::time::timeout(Duration::from_secs(30), node.next_event_async()).await {
			Ok(event) => match event {
				ldk_node::Event::ChannelClosed { .. }
				| ldk_node::Event::PaymentFailed { .. }
				| ldk_node::Event::PaymentSuccessful { .. } => {
					node.event_handled().unwrap();
				},
				_ => {
					node.event_handled().unwrap();
					break;
				},
			},
			Err(_) => break, // No more events within timeout
		}
	}
}

// ---------------------------------------------------------------------------
// Combined proptest scenario
// ---------------------------------------------------------------------------

/// Run a combined interop scenario with randomized parameters.
pub(crate) async fn run_interop_property_test<E: ElectrumApi>(
	node: &Node, peer: &(impl ExternalNode + ?Sized), bitcoind: &BitcoindClient, electrs: &E,
	disconnect_phase: Phase, disconnect_initiator: Side, close_type: CloseType,
	close_initiator: Side, payment_type: PayType,
) {
	// Setup: fund + connect
	setup_interop_test(node, peer, bitcoind, electrs).await;

	// Open channel
	let (user_channel_id, ext_channel_id) =
		open_channel_to_external(node, peer, bitcoind, electrs, 1_000_000, Some(500_000_000)).await;

	// Phase 1: Disconnect/Reconnect at the specified phase
	match disconnect_phase {
		Phase::Idle => {
			test_disconnect_reconnect_idle(node, peer, bitcoind, electrs, &disconnect_initiator)
				.await;
		},
		Phase::Payment => {
			test_disconnect_during_payment(node, peer, bitcoind, electrs, &disconnect_initiator)
				.await;
		},
		Phase::ChannelOpen | Phase::Close => {
			// Simplified: do idle disconnect for these phases
			test_disconnect_reconnect_idle(node, peer, bitcoind, electrs, &disconnect_initiator)
				.await;
		},
	}

	// Phase 2: Make a payment
	let ext_node_id = peer.get_node_id().await.unwrap();
	match payment_type {
		PayType::Bolt11 => {
			let invoice_str = peer.create_invoice(5_000_000, "proptest-bolt11").await.unwrap();
			let parsed = Bolt11Invoice::from_str(&invoice_str).unwrap();
			node.bolt11_payment().send(&parsed, None).unwrap();
			expect_event!(node, PaymentSuccessful);
		},
		PayType::Keysend => {
			node.spontaneous_payment().send(5_000_000, ext_node_id, None).unwrap();
			expect_event!(node, PaymentSuccessful);
		},
	}

	// Phase 3: Close channel
	match (&close_type, &close_initiator) {
		(CloseType::Cooperative, Side::Ldk) => {
			test_cooperative_close_by_ldk(node, peer, &user_channel_id).await;
		},
		(CloseType::Cooperative, Side::External) => {
			test_cooperative_close_by_external(node, peer, &ext_channel_id).await;
		},
		(CloseType::Force, Side::Ldk) => {
			test_force_close_by_ldk(node, peer, bitcoind, electrs, &user_channel_id).await;
		},
		(CloseType::Force, Side::External) => {
			test_force_close_by_external(node, peer, bitcoind, electrs, &ext_channel_id).await;
		},
	}
}
