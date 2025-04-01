// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

//! Helper to process PSBTSent/PSBTReceived events.

use std::sync::Arc;

use bitcoin::psbt::{Input, Output};
use bitcoin::{Amount, Psbt, TxIn, TxOut};
use lightning::events::Event as LdkEvent;
use rand::seq::SliceRandom;
use rand::thread_rng;

use crate::config::Config;
use crate::types::{ChannelManager, Wallet};

pub(crate) fn process_batch_events(
	event: LdkEvent, config: &Arc<Config>, channel_manager: &Arc<ChannelManager>,
	wallet: &Arc<Wallet>,
) {
	let mut alias = "NO_ALIAS".to_string();
	if let Some(node_alias) = config.node_alias {
		alias = node_alias.to_string();
	}
	match event {
		LdkEvent::PSBTSent {
			next_node_id,
			uniform_amount,
			fee_per_participant,
			max_participants,
			participants,
			hops,
			psbt_hex,
			sign,
		} => {
			println!(
        "[{}] PSBTSent    : next_node={} | uni_amount={} | fee={} | max_p={} | participants={} | hops={} | len={} | sign={}",
        alias,
        next_node_id,
        uniform_amount,
        fee_per_participant,
        max_participants,
        participants.len(),
        hops.len(),
        psbt_hex.len(),
        sign,
      );
		},
		LdkEvent::PSBTReceived {
			receiver_node_id,
			prev_node_id,
			uniform_amount,
			fee_per_participant,
			max_participants,
			participants,
			hops,
			psbt_hex,
			sign,
		} => {
			println!(
        "[{}] PSBTReceived: prev_node={} | uni_amount={} | fee={} | max_p={} | participants={} | hops={} | len={} | sign={}",
        alias,
        prev_node_id,
        uniform_amount,
        fee_per_participant,
        max_participants,
        participants.len(),
        hops.len(),
        psbt_hex.len(),
        sign,
      );

			let mut psbt = Psbt::deserialize(&hex::decode(psbt_hex).unwrap()).unwrap();

			let mut hops = hops;
			let mut participants = participants;

			// Not a participant yet
			if !sign && !participants.contains(&receiver_node_id) {
				participants.push(receiver_node_id);
				// Add node's inputs/outputs and route it to the next node
				let fee = Amount::from_sat(fee_per_participant);

				let uniform_amount_opt =
					if uniform_amount > 0 { Some(Amount::from_sat(uniform_amount)) } else { None };

				wallet.add_utxos_to_psbt(&mut psbt, 2, uniform_amount_opt, fee, false).unwrap();
			}

			let mut sign = sign;
			if (participants.len() as u8) >= max_participants {
				sign = true;

				// Shuffling inputs/outputs
				println!("\n[{}] PSBTReceived: Shuffling inputs/outputs before starting the Signing workflow...", alias);
				let mut rng = thread_rng();
				let mut paired_inputs: Vec<(Input, TxIn)> = psbt
					.inputs
					.iter()
					.cloned()
					.zip(psbt.unsigned_tx.input.iter().cloned())
					.collect();
				paired_inputs.shuffle(&mut rng);

				// Unzip the shuffled pairs back into psbt
				let (shuffled_psbt_inputs, shuffled_tx_inputs): (Vec<_>, Vec<_>) =
					paired_inputs.into_iter().unzip();
				psbt.inputs = shuffled_psbt_inputs;
				psbt.unsigned_tx.input = shuffled_tx_inputs;

				// Step 2: Shuffle outputs while keeping psbt.outputs and psbt.unsigned_tx.output aligned
				let mut paired_outputs: Vec<(Output, TxOut)> = psbt
					.outputs
					.iter()
					.cloned()
					.zip(psbt.unsigned_tx.output.iter().cloned())
					.collect();
				paired_outputs.shuffle(&mut rng);

				// Unzip the shuffled pairs back into psbt
				let (shuffled_psbt_outputs, shuffled_tx_outputs): (Vec<_>, Vec<_>) =
					paired_outputs.into_iter().unzip();
				psbt.outputs = shuffled_psbt_outputs;
				psbt.unsigned_tx.output = shuffled_tx_outputs;

				println!("\n[{}] PSBTReceived: Starting the Signing workflow (send final PSBT back to initial node)...\n", alias);
			}

			let open_channels = channel_manager.list_channels();

			if !sign {
				let mut next_node_id = None;
				for channel_details in &open_channels {
					if participants.contains(&channel_details.counterparty.node_id) {
						continue;
					}
					if hops.last().unwrap() == &channel_details.counterparty.node_id {
						continue;
					}
					next_node_id = Some(channel_details.counterparty.node_id);
					break;
				}

				// We are already a participant/hop and all our peers are too, so we need to route the PSBT back to someone else
				if next_node_id.is_none() {
					if open_channels.len() == 1 {
						next_node_id = Some(open_channels[0].counterparty.node_id);
					}
					for channel_details in open_channels {
						if channel_details.counterparty.node_id == prev_node_id {
							continue;
						}
						next_node_id = Some(channel_details.counterparty.node_id);
						break;
					}
				}

				hops.push(receiver_node_id);
				let psbt_hex = psbt.serialize_hex();

				let _ = channel_manager.send_psbt(
					next_node_id.unwrap(),
					uniform_amount,
					fee_per_participant,
					max_participants,
					participants.clone(),
					hops.clone(),
					psbt_hex,
					false,
				);
			} else {
				// Check if we need to sign or just route the PSBT to someone else
				if participants.contains(&receiver_node_id) {
					println!("[{}] PSBTReceived: Signing...", alias);
					wallet.payjoin_sign_psbt(&mut psbt).unwrap();
					participants.retain(|key| *key != receiver_node_id);
				}

				let psbt_hex = psbt.serialize_hex();

				// Do we need more signatures?
				if hops.len() > 0 {
					let next_signer_node_id = hops.pop().unwrap();
					if channel_manager.list_channels_with_counterparty(&next_signer_node_id).len()
						> 0
					{
						let _ = channel_manager.send_psbt(
							next_signer_node_id,
							uniform_amount,
							fee_per_participant,
							max_participants,
							participants,
							hops,
							psbt_hex,
							true,
						);
					} else {
						let mut inner_participants = participants.clone();
						for node_id in participants.iter().rev() {
							if channel_manager.list_channels_with_counterparty(&node_id).len() > 0 {
								// We need to add back the next_signer_node_id to participants
								if !inner_participants.contains(&next_signer_node_id) {
									inner_participants.push(next_signer_node_id);
								}
								let _ = channel_manager.send_psbt(
									node_id.clone(),
									uniform_amount,
									fee_per_participant,
									max_participants,
									inner_participants,
									hops,
									psbt_hex,
									true,
								);
								break;
							}
						}
					}
				} else {
					println!(
						"[{}] PSBTReceived: PSBT was signed by all participants! (len={})",
						alias,
						psbt_hex.len()
					);
					wallet.push_to_batch_psbts(psbt_hex).unwrap();
				}
			}
		},
		_ => {},
	}
}
