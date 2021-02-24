// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

use bitcoin::blockdata::script::Builder;
use bitcoin::blockdata::transaction::TxOut;
use bitcoin::hash_types::BlockHash;

use lightning::chain;
use lightning::ln::channelmanager::ChannelDetails;
use lightning::ln::features::InitFeatures;
use lightning::ln::msgs;
use lightning::routing::router::{get_route, RouteHint};
use lightning::util::logger::Logger;
use lightning::util::ser::Readable;
use lightning::routing::network_graph::{NetworkGraph, RoutingFees};

use bitcoin::secp256k1::key::PublicKey;
use bitcoin::network::constants::Network;
use bitcoin::blockdata::constants::genesis_block;

use utils::test_logger;

use std::collections::{HashSet, HashMap};
use std::hash;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[inline]
pub fn slice_to_be16(v: &[u8]) -> u16 {
	((v[0] as u16) << 8*1) |
	((v[1] as u16) << 8*0)
}

#[inline]
pub fn slice_to_be32(v: &[u8]) -> u32 {
	((v[0] as u32) << 8*3) |
	((v[1] as u32) << 8*2) |
	((v[2] as u32) << 8*1) |
	((v[3] as u32) << 8*0)
}

#[inline]
pub fn slice_to_be64(v: &[u8]) -> u64 {
	((v[0] as u64) << 8*7) |
	((v[1] as u64) << 8*6) |
	((v[2] as u64) << 8*5) |
	((v[3] as u64) << 8*4) |
	((v[4] as u64) << 8*3) |
	((v[5] as u64) << 8*2) |
	((v[6] as u64) << 8*1) |
	((v[7] as u64) << 8*0)
}


struct InputData {
	data: Vec<u8>,
	read_pos: AtomicUsize,
}
impl InputData {
	fn get_slice(&self, len: usize) -> Option<&[u8]> {
		let old_pos = self.read_pos.fetch_add(len, Ordering::AcqRel);
		if self.data.len() < old_pos + len {
			return None;
		}
		Some(&self.data[old_pos..old_pos + len])
	}
	fn get_slice_nonadvancing(&self, len: usize) -> Option<&[u8]> {
		let old_pos = self.read_pos.load(Ordering::Acquire);
		if self.data.len() < old_pos + len {
			return None;
		}
		Some(&self.data[old_pos..old_pos + len])
	}
}

struct FuzzChainSource {
	input: Arc<InputData>,
}
impl chain::Access for FuzzChainSource {
	fn get_utxo(&self, _genesis_hash: &BlockHash, _short_channel_id: u64) -> Result<TxOut, chain::AccessError> {
		match self.input.get_slice(2) {
			Some(&[0, _]) => Err(chain::AccessError::UnknownChain),
			Some(&[1, _]) => Err(chain::AccessError::UnknownTx),
			Some(&[_, x]) => Ok(TxOut { value: 0, script_pubkey: Builder::new().push_int(x as i64).into_script().to_v0_p2wsh() }),
			None => Err(chain::AccessError::UnknownTx),
			_ => unreachable!(),
		}
	}
}

// We sometimes walk the HashSet of peer node_ids, which, in order to keep the ordering consistent
// across fuzz runs, we need to use a consistent hasher.
// Tt is deprecated, but the "replacement" doesn't actually accomplish the same goals, so we just
// ignore it.
#[allow(deprecated)]
pub type NonRandomHash = hash::BuildHasherDefault<hash::SipHasher>;

#[inline]
pub fn do_test<Out: test_logger::Output>(data: &[u8], out: Out) {
	let input = Arc::new(InputData {
		data: data.to_vec(),
		read_pos: AtomicUsize::new(0),
	});
	macro_rules! get_slice_nonadvancing {
		($len: expr) => {
			match input.get_slice_nonadvancing($len as usize) {
				Some(slice) => slice,
				None => return,
			}
		}
	}
	macro_rules! get_slice {
		($len: expr) => {
			match input.get_slice($len as usize) {
				Some(slice) => slice,
				None => return,
			}
		}
	}

	macro_rules! decode_msg {
		($MsgType: path, $len: expr) => {{
			let mut reader = ::std::io::Cursor::new(get_slice!($len));
			match <$MsgType>::read(&mut reader) {
				Ok(msg) => {
					assert_eq!(reader.position(), $len as u64);
					msg
				},
				Err(e) => match e {
					msgs::DecodeError::UnknownVersion => return,
					msgs::DecodeError::UnknownRequiredFeature => return,
					msgs::DecodeError::InvalidValue => return,
					msgs::DecodeError::BadLengthDescriptor => return,
					msgs::DecodeError::ShortRead => panic!("We picked the length..."),
					msgs::DecodeError::Io(e) => panic!(format!("{}", e)),
				}
			}
		}}
	}

	macro_rules! decode_msg_with_len16 {
		($MsgType: path, $excess: expr) => {
			{
				let extra_len = slice_to_be16(get_slice_nonadvancing!(2));
				decode_msg!($MsgType, 2 + (extra_len as usize) + $excess)
			}
		}
	}

	macro_rules! get_pubkey {
		() => {
			match PublicKey::from_slice(get_slice!(33)) {
				Ok(key) => key,
				Err(_) => return,
			}
		}
	}

	let logger: Arc<dyn Logger> = Arc::new(test_logger::TestLogger::new("".to_owned(), out));

	let our_pubkey = get_pubkey!();
	let mut net_graph = NetworkGraph::new(genesis_block(Network::Bitcoin).header.block_hash());

	let mut node_pks: HashSet<_, NonRandomHash> = HashSet::default();
	let mut scid = 42;

	let mut channel_limits = HashMap::new();

	loop {
		match get_slice!(1)[0] {
			0 => {
				let start_len = slice_to_be16(&get_slice_nonadvancing!(2)[0..2]) as usize;
				let addr_len = slice_to_be16(&get_slice_nonadvancing!(start_len+2 + 74)[start_len+2 + 72..start_len+2 + 74]);
				if addr_len > (37+1)*4 {
					return;
				}
				let msg = decode_msg_with_len16!(msgs::UnsignedNodeAnnouncement, 288);
				node_pks.insert(msg.node_id);
				let _ = net_graph.update_node_from_unsigned_announcement(&msg);
			},
			1 => {
				let msg = decode_msg_with_len16!(msgs::UnsignedChannelAnnouncement, 32+8+33*4);
				node_pks.insert(msg.node_id_1);
				node_pks.insert(msg.node_id_2);
				let _ = net_graph.update_channel_from_unsigned_announcement::<&FuzzChainSource>(&msg, &None);
			},
			2 => {
				let msg = decode_msg_with_len16!(msgs::UnsignedChannelAnnouncement, 32+8+33*4);
				node_pks.insert(msg.node_id_1);
				node_pks.insert(msg.node_id_2);
				let _ = net_graph.update_channel_from_unsigned_announcement(&msg, &Some(&FuzzChainSource { input: Arc::clone(&input) }));
			},
			3 => {
				let msg = decode_msg!(msgs::UnsignedChannelUpdate, 72);
				if net_graph.update_channel_unsigned(&msg).is_ok() {
					channel_limits.insert((msg.short_channel_id, if msg.flags & 1 == 1 { true } else { false }), msg);
				}
			},
			4 => {
				let short_channel_id = slice_to_be64(get_slice!(8));
				net_graph.close_channel_from_update(short_channel_id, false);
				channel_limits.remove(&(short_channel_id, true));
				channel_limits.remove(&(short_channel_id, false));
			},
			_ if node_pks.is_empty() => {},
			_ => {
				let mut first_hops_vec = Vec::new();
				let first_hops = match get_slice!(1)[0] {
					0 => None,
					count => {
						for _ in 0..count {
							scid += 1;
							let rnid = node_pks.iter().skip(slice_to_be16(get_slice!(2)) as usize % node_pks.len()).next().unwrap();
							first_hops_vec.push(ChannelDetails {
								channel_id: [0; 32],
								short_channel_id: Some(scid),
								remote_network_id: *rnid,
								counterparty_features: InitFeatures::known(),
								channel_value_satoshis: slice_to_be64(get_slice!(8)),
								user_id: 0,
								inbound_capacity_msat: 0,
								is_live: true,
								outbound_capacity_msat: 0,
							});
						}
						Some(&first_hops_vec[..])
					},
				};
				let mut last_hops_vec = Vec::new();
				{
					let count = get_slice!(1)[0];
					for _ in 0..count {
						scid += 1;
						let rnid = node_pks.iter().skip(slice_to_be16(get_slice!(2)) as usize % node_pks.len()).next().unwrap();
						last_hops_vec.push(RouteHint {
							src_node_id: *rnid,
							short_channel_id: scid,
							fees: RoutingFees {
								base_msat: slice_to_be32(get_slice!(4)),
								proportional_millionths: slice_to_be32(get_slice!(4)),
							},
							cltv_expiry_delta: slice_to_be16(get_slice!(2)),
							htlc_minimum_msat: Some(slice_to_be64(get_slice!(8))),
							htlc_maximum_msat: None,
						});
					}
				}
				let last_hops = &last_hops_vec[..];
				for target in node_pks.iter() {
					let value_msat = slice_to_be64(get_slice!(8));
					let cltv = slice_to_be32(get_slice!(4));
					if let Ok(route) = get_route(&our_pubkey, &net_graph, target,
							first_hops.map(|c| c.iter().collect::<Vec<_>>()).as_ref().map(|a| a.as_slice()),
							&last_hops.iter().collect::<Vec<_>>(),
							value_msat, cltv, Arc::clone(&logger)) {
						let mut sent_msat = 0;
						'path_l: for (idxp, path) in route.paths.iter().enumerate() {
							sent_msat += path.last().unwrap().fee_msat;
							assert_eq!(path.last().unwrap().cltv_expiry_delta, cltv);

							if value_msat == 0 { continue 'path_l; }

							let mut path_total_msat = path.last().unwrap().fee_msat;
							for (idx, first_prev_hop) in path.windows(2).enumerate().rev() {
								let (prev_hop, hop) = (&first_prev_hop[0], &first_prev_hop[1]);
								let (min, max, expiry, fees) = 'find_chan_loop: loop {
									if idx == 0 {
										if let Some(hops) = first_hops {
											for first_hop in hops {
												if first_hop.short_channel_id == Some(hop.short_channel_id) {
													break 'find_chan_loop (None, Some(first_hop.outbound_capacity_msat),
														0, RoutingFees { base_msat: 0, proportional_millionths: 0 });
												}
											}
										}
									}
									if idx == path.len() - 2 {
										for last_hop in last_hops {
											if last_hop.short_channel_id == hop.short_channel_id {
												break 'find_chan_loop (last_hop.htlc_minimum_msat,
													last_hop.htlc_maximum_msat, last_hop.cltv_expiry_delta,
													last_hop.fees);
											}
										}
									}
									// We don't know by looking at a route whether the inbound or outbound
									// direction is in use, so we only test if we only have one filled in.
									let upd_a = channel_limits.get(&(hop.short_channel_id, false));
									let upd_b = channel_limits.get(&(hop.short_channel_id, true));
									if upd_a.is_some() && upd_b.is_some() { continue 'path_l; }
									let upd = if let Some(u) = upd_a { u } else if let Some(u) = upd_b { u } else { panic!(); };
									break 'find_chan_loop (Some(upd.htlc_minimum_msat),
										match upd.htlc_maximum_msat {
											msgs::OptionalField::Absent => None,
											msgs::OptionalField::Present(v) => Some(v),
										}, upd.cltv_expiry_delta,
										RoutingFees { base_msat: upd.fee_base_msat,
											proportional_millionths: upd.fee_proportional_millionths });
								};


								if let Some(v) = max {
									assert!(path_total_msat <= v);
								}
								if let Some(v) = min {
									assert!(path_total_msat >= v);
								}
								assert!(prev_hop.fee_msat >= fees.base_msat as u64);
								assert_eq!(prev_hop.fee_msat, fees.base_msat as u64 + fees.proportional_millionths as u64 * path_total_msat / 1_000_000);
								path_total_msat += prev_hop.fee_msat;
								assert_eq!(prev_hop.cltv_expiry_delta, expiry as u32);
							}
						}
						assert_eq!(sent_msat, value_msat);
					}
				}
			},
		}
	}
}

pub fn router_test<Out: test_logger::Output>(data: &[u8], out: Out) {
	do_test(data, out);
}

#[no_mangle]
pub extern "C" fn router_run(data: *const u8, datalen: usize) {
	do_test(unsafe { std::slice::from_raw_parts(data, datalen) }, test_logger::DevNull {});
}
