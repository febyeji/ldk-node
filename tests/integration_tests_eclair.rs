// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

#![cfg(eclair_test)]

mod common;

use std::str::FromStr;

use electrsd::corepc_client::client_sync::Auth;
use electrsd::corepc_node::Client as BitcoindClient;
use electrum_client::Client as ElectrumClient;
use ldk_node::{Builder, Event};
use proptest::prelude::*;
use proptest::proptest;

use common::eclair::TestEclairNode;
use common::external_node::ExternalNode;
use common::scenarios::*;

fn setup_clients() -> (BitcoindClient, ElectrumClient, TestEclairNode) {
	let bitcoind = BitcoindClient::new_with_auth(
		"http://127.0.0.1:18443",
		Auth::UserPass("user".to_string(), "pass".to_string()),
	)
	.unwrap();
	let electrs = ElectrumClient::new("tcp://127.0.0.1:50001").unwrap();

	let eclair = TestEclairNode::from_env();
	(bitcoind, electrs, eclair)
}

fn setup_ldk_node() -> ldk_node::Node {
	let config = common::random_config(true);
	let mut builder = Builder::from_config(config.node_config);
	builder.set_chain_source_esplora("http://127.0.0.1:3002".to_string(), None);
	let node = builder.build(config.node_entropy).unwrap();
	node.start().unwrap();
	node
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_eclair_basic_channel_cycle() {
	let (bitcoind, electrs, eclair) = setup_clients();
	common::generate_blocks_and_wait(&bitcoind, &electrs, 1).await;

	let node = setup_ldk_node();
	setup_interop_test(&node, &eclair, &bitcoind, &electrs).await;

	let (user_channel_id, _ext_channel_id) =
		open_channel_to_external(&node, &eclair, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	// LDK -> Eclair payment
	let invoice = eclair.create_invoice(10_000_000, "eclair-test-send").await.unwrap();
	let parsed = lightning_invoice::Bolt11Invoice::from_str(&invoice).unwrap();
	node.bolt11_payment().send(&parsed, None).unwrap();
	common::expect_event!(node, PaymentSuccessful);

	// Eclair -> LDK payment
	let ldk_invoice = node
		.bolt11_payment()
		.receive(
			10_000_000,
			&lightning_invoice::Bolt11InvoiceDescription::Direct(
				lightning_invoice::Description::new("eclair-test-recv".to_string()).unwrap(),
			),
			3600,
		)
		.unwrap();
	eclair.pay_invoice(&ldk_invoice.to_string()).await.unwrap();
	common::expect_event!(node, PaymentReceived);

	test_cooperative_close_by_ldk(&node, &eclair, &user_channel_id).await;
	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_eclair_disconnect_reconnect() {
	let (bitcoind, electrs, eclair) = setup_clients();
	common::generate_blocks_and_wait(&bitcoind, &electrs, 1).await;

	let node = setup_ldk_node();
	setup_interop_test(&node, &eclair, &bitcoind, &electrs).await;
	let (_user_ch, _ext_ch) =
		open_channel_to_external(&node, &eclair, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	test_disconnect_reconnect_idle(&node, &eclair, &bitcoind, &electrs, &Side::Ldk).await;
	test_disconnect_reconnect_idle(&node, &eclair, &bitcoind, &electrs, &Side::External).await;

	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_eclair_force_close_by_ldk() {
	let (bitcoind, electrs, eclair) = setup_clients();
	common::generate_blocks_and_wait(&bitcoind, &electrs, 1).await;

	let node = setup_ldk_node();
	setup_interop_test(&node, &eclair, &bitcoind, &electrs).await;
	let (user_ch, _ext_ch) =
		open_channel_to_external(&node, &eclair, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	test_force_close_by_ldk(&node, &eclair, &bitcoind, &electrs, &user_ch).await;
	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_eclair_force_close_by_external() {
	let (bitcoind, electrs, eclair) = setup_clients();
	common::generate_blocks_and_wait(&bitcoind, &electrs, 1).await;

	let node = setup_ldk_node();
	setup_interop_test(&node, &eclair, &bitcoind, &electrs).await;
	let (_user_ch, ext_ch) =
		open_channel_to_external(&node, &eclair, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	test_force_close_by_external(&node, &eclair, &bitcoind, &electrs, &ext_ch).await;
	node.stop().unwrap();
}

proptest! {
	#![proptest_config(proptest::test_runner::Config::with_cases(8))]
	#[test]
	fn test_eclair_interop_proptest(
		disconnect_phase in prop_oneof![
			Just(Phase::ChannelOpen),
			Just(Phase::Payment),
			Just(Phase::Close),
			Just(Phase::Idle),
		],
		disconnect_initiator in prop_oneof![Just(Side::Ldk), Just(Side::External)],
		close_type in prop_oneof![Just(CloseType::Cooperative), Just(CloseType::Force)],
		close_initiator in prop_oneof![Just(Side::Ldk), Just(Side::External)],
		payment_type in prop_oneof![Just(PayType::Bolt11), Just(PayType::Keysend)],
	) {
		let rt = tokio::runtime::Builder::new_multi_thread()
			.enable_all()
			.build()
			.unwrap();
		rt.block_on(async {
			let (bitcoind, electrs, eclair) = setup_clients();
			common::generate_blocks_and_wait(&bitcoind, &electrs, 1).await;

			let node = setup_ldk_node();
			run_interop_property_test(
				&node, &eclair, &bitcoind, &electrs,
				disconnect_phase, disconnect_initiator, close_type,
				close_initiator, payment_type,
			).await;
			node.stop().unwrap();
		});
	}
}
