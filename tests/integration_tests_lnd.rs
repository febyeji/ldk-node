// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

#![cfg(lnd_test)]

mod common;

use std::str::FromStr;

use electrsd::corepc_client::client_sync::Auth;
use electrsd::corepc_node::Client as BitcoindClient;
use electrum_client::Client as ElectrumClient;
use ldk_node::{Builder, Event};
use proptest::prelude::*;
use proptest::proptest;

use common::external_node::ExternalNode;
use common::lnd::TestLndNode;
use common::scenarios::*;

async fn setup_clients() -> (BitcoindClient, ElectrumClient, TestLndNode) {
	let bitcoind = BitcoindClient::new_with_auth(
		"http://127.0.0.1:18443",
		Auth::UserPass("user".to_string(), "pass".to_string()),
	)
	.unwrap();
	let electrs = ElectrumClient::new("tcp://127.0.0.1:50001").unwrap();
	let lnd = TestLndNode::from_env().await;
	(bitcoind, electrs, lnd)
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
async fn test_lnd_basic_channel_cycle() {
	let (bitcoind, electrs, lnd) = setup_clients().await;
	common::generate_blocks_and_wait(&bitcoind, &electrs, 1).await;

	let node = setup_ldk_node();
	setup_interop_test(&node, &lnd, &bitcoind, &electrs).await;

	let (user_channel_id, _ext_channel_id) =
		open_channel_to_external(&node, &lnd, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	// LDK -> LND payment
	let invoice = lnd.create_invoice(100_000_000, "lnd-test-send").await.unwrap();
	let parsed = lightning_invoice::Bolt11Invoice::from_str(&invoice).unwrap();
	node.bolt11_payment().send(&parsed, None).unwrap();
	common::expect_event!(node, PaymentSuccessful);

	// LND -> LDK payment (wait for routing sync)
	tokio::time::sleep(std::time::Duration::from_secs(2)).await;
	let ldk_invoice = node
		.bolt11_payment()
		.receive(
			9_000_000,
			&lightning_invoice::Bolt11InvoiceDescription::Direct(
				lightning_invoice::Description::new("lnd-test-recv".to_string()).unwrap(),
			),
			3600,
		)
		.unwrap();
	lnd.pay_invoice(&ldk_invoice.to_string()).await.unwrap();
	common::expect_event!(node, PaymentReceived);

	test_cooperative_close_by_ldk(&node, &lnd, &user_channel_id).await;
	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_lnd_disconnect_reconnect() {
	let (bitcoind, electrs, lnd) = setup_clients().await;
	common::generate_blocks_and_wait(&bitcoind, &electrs, 1).await;

	let node = setup_ldk_node();
	setup_interop_test(&node, &lnd, &bitcoind, &electrs).await;
	let (_user_ch, _ext_ch) =
		open_channel_to_external(&node, &lnd, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	test_disconnect_reconnect_idle(&node, &lnd, &bitcoind, &electrs, &Side::Ldk).await;
	test_disconnect_reconnect_idle(&node, &lnd, &bitcoind, &electrs, &Side::External).await;

	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_lnd_force_close_by_ldk() {
	let (bitcoind, electrs, lnd) = setup_clients().await;
	common::generate_blocks_and_wait(&bitcoind, &electrs, 1).await;

	let node = setup_ldk_node();
	setup_interop_test(&node, &lnd, &bitcoind, &electrs).await;
	let (user_ch, _ext_ch) =
		open_channel_to_external(&node, &lnd, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	test_force_close_by_ldk(&node, &lnd, &bitcoind, &electrs, &user_ch).await;
	node.stop().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_lnd_force_close_by_external() {
	let (bitcoind, electrs, lnd) = setup_clients().await;
	common::generate_blocks_and_wait(&bitcoind, &electrs, 1).await;

	let node = setup_ldk_node();
	setup_interop_test(&node, &lnd, &bitcoind, &electrs).await;
	let (_user_ch, ext_ch) =
		open_channel_to_external(&node, &lnd, &bitcoind, &electrs, 1_000_000, Some(500_000_000))
			.await;

	test_force_close_by_external(&node, &lnd, &bitcoind, &electrs, &ext_ch).await;
	node.stop().unwrap();
}

proptest! {
	#![proptest_config(proptest::test_runner::Config::with_cases(8))]
	#[test]
	fn test_lnd_interop_proptest(
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
			let (bitcoind, electrs, lnd) = setup_clients().await;
			common::generate_blocks_and_wait(&bitcoind, &electrs, 1).await;

			let node = setup_ldk_node();
			run_interop_property_test(
				&node, &lnd, &bitcoind, &electrs,
				disconnect_phase, disconnect_initiator, close_type,
				close_initiator, payment_type,
			).await;
			node.stop().unwrap();
		});
	}
}
