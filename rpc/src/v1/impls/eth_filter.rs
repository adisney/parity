// Copyright 2015-2017 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! Eth Filter RPC implementation

use std::sync::Arc;
use std::collections::HashSet;
use std::result;

use ethcore::miner::{self, MinerService};
use ethcore::filter::Filter as EthcoreFilter;
use ethcore::client::{BlockChainClient, BlockId};
use ethcore::executed::{Executed, CallError};
use ethcore::call_analytics::CallAnalytics;
use ethcore::encoded;
use ethereum_types::H256;
use parking_lot::Mutex;

use jsonrpc_core::{BoxFuture, Result};
use jsonrpc_core::futures::{future, Future};
use jsonrpc_core::futures::future::{Either, join_all};
use v1::traits::EthFilter;
use v1::types::{BlockNumber, Index, Filter, FilterChanges, Log, H256 as RpcH256, U256 as RpcU256, ReturnData, Bytes};
use v1::helpers::{errors, PollFilter, PollManager, limit_logs};
use v1::impls::eth::pending_logs;

/// Something which provides data that can be filtered over.
pub trait Filterable {
	/// Current best block number.
	fn best_block_number(&self) -> u64;

	/// Get a block hash by block id.
	fn block_hash(&self, id: BlockId) -> Option<RpcH256>;

	/// Get a block body by block id.
	fn block_body(&self, id: BlockId) -> BoxFuture<Option<encoded::Body>>;

	/// pending transaction hashes at the given block.
	fn pending_transactions_hashes(&self) -> Vec<H256>;

	/// Get logs that match the given filter.
	fn logs(&self, filter: EthcoreFilter) -> BoxFuture<Vec<Log>>;

	/// Get logs from the pending block.
	fn pending_logs(&self, block_number: u64, filter: &EthcoreFilter) -> Vec<Log>;

	/// Get a reference to the poll manager.
	fn polls(&self) -> &Mutex<PollManager<PollFilter>>;

	/// Replay the transactions from the specified block
	fn replay_block_transactions(&self, block: BlockId) -> Result<result::Result<Box<Iterator<Item = Executed>>, CallError>>;
}

/// Eth filter rpc implementation for a full node.
pub struct EthFilterClient<C, M> {
	client: Arc<C>,
	miner: Arc<M>,
	polls: Mutex<PollManager<PollFilter>>,
}

impl<C, M> EthFilterClient<C, M> {
	/// Creates new Eth filter client.
	pub fn new(client: Arc<C>, miner: Arc<M>) -> Self {
		EthFilterClient {
			client: client,
			miner: miner,
			polls: Mutex::new(PollManager::new()),
		}
	}
}

impl<C, M> Filterable for EthFilterClient<C, M> where
	C: miner::BlockChainClient + BlockChainClient,
	M: MinerService,
{
	fn best_block_number(&self) -> u64 {
		self.client.chain_info().best_block_number
	}

	fn block_hash(&self, id: BlockId) -> Option<RpcH256> {
		self.client.block_hash(id).map(Into::into)
	}

	fn block_body(&self, id: BlockId) -> BoxFuture<Option<encoded::Body>> {
		Box::new(future::ok(self.client.block_body(id)))
	}

	fn pending_transactions_hashes(&self) -> Vec<H256> {
		self.miner.ready_transactions(&*self.client)
			.into_iter()
			.map(|tx| tx.signed().hash())
			.collect()
	}

	fn logs(&self, filter: EthcoreFilter) -> BoxFuture<Vec<Log>> {
		Box::new(future::ok(self.client.logs(filter).into_iter().map(Into::into).collect()))
	}

	fn pending_logs(&self, block_number: u64, filter: &EthcoreFilter) -> Vec<Log> {
		pending_logs(&*self.miner, block_number, filter)
	}

	fn polls(&self) -> &Mutex<PollManager<PollFilter>> { &self.polls }

	fn replay_block_transactions(&self, block: BlockId) -> Result<result::Result<Box<Iterator<Item = Executed>>, CallError>> {
		Ok(self.client.replay_block_transactions(block, CallAnalytics { transaction_tracing: false, vm_tracing: false, state_diffing: false}))
	}
}

impl<T: Filterable + Send + Sync + 'static> EthFilter for T {
	fn new_filter(&self, filter: Filter) -> Result<RpcU256> {
		let mut polls = self.polls().lock();
		let block_number = self.best_block_number();
		let id = polls.create_poll(PollFilter::Logs(block_number, Default::default(), filter));
		Ok(id.into())
	}

	fn new_block_filter(&self) -> Result<RpcU256> {
		let mut polls = self.polls().lock();
		// +1, since we don't want to include the current block
		let id = polls.create_poll(PollFilter::Block(self.best_block_number() + 1));
		Ok(id.into())
	}

	fn new_pending_transaction_filter(&self) -> Result<RpcU256> {
		let mut polls = self.polls().lock();
		let pending_transactions = self.pending_transactions_hashes();
		let id = polls.create_poll(PollFilter::PendingTransaction(pending_transactions));
		Ok(id.into())
	}

	fn new_return_data_filter(&self) -> Result<RpcU256> {
		let mut polls = self.polls().lock();
		let id = polls.create_poll(PollFilter::ReturnData(self.best_block_number()));
		Ok(id.into())
	}

	fn filter_changes(&self, index: Index) -> BoxFuture<FilterChanges> {
		let mut polls = self.polls().lock();
		Box::new(match polls.poll_mut(&index.value()) {
			None => Either::A(future::err(errors::filter_not_found())),
			Some(filter) => match *filter {
				PollFilter::Block(ref mut block_number) => {
					// +1, cause we want to return hashes including current block hash.
					let current_number = self.best_block_number() + 1;
					let hashes = (*block_number..current_number).into_iter()
						.map(BlockId::Number)
						.filter_map(|id| self.block_hash(id))
						.collect::<Vec<RpcH256>>();

					*block_number = current_number;

					Either::A(future::ok(FilterChanges::Hashes(hashes)))
				},
				PollFilter::PendingTransaction(ref mut previous_hashes) => {
					// get hashes of pending transactions
					let current_hashes = self.pending_transactions_hashes();

					let new_hashes =
					{
						let previous_hashes_set = previous_hashes.iter().collect::<HashSet<_>>();

						//	find all new hashes
						current_hashes
							.iter()
							.filter(|hash| !previous_hashes_set.contains(hash))
							.cloned()
							.map(Into::into)
							.collect::<Vec<RpcH256>>()
					};

					// save all hashes of pending transactions
					*previous_hashes = current_hashes;

					// return new hashes
					Either::A(future::ok(FilterChanges::Hashes(new_hashes)))
				},
				PollFilter::Logs(ref mut block_number, ref mut previous_logs, ref filter) => {
					// retrive the current block number
					let current_number = self.best_block_number();

					// check if we need to check pending hashes
					let include_pending = filter.to_block == Some(BlockNumber::Pending);

					// build appropriate filter
					let mut filter: EthcoreFilter = filter.clone().into();
					filter.from_block = BlockId::Number(*block_number);
					filter.to_block = BlockId::Latest;

					// retrieve pending logs
					let pending = if include_pending {
						let pending_logs = self.pending_logs(current_number, &filter);

						// remove logs about which client was already notified about
						let new_pending_logs: Vec<_> = pending_logs.iter()
							.filter(|p| !previous_logs.contains(p))
							.cloned()
							.collect();

						// save all logs retrieved by client
						*previous_logs = pending_logs.into_iter().collect();

						new_pending_logs
					} else {
						Vec::new()
					};

					// save the number of the next block as a first block from which
					// we want to get logs
					*block_number = current_number + 1;

					// retrieve logs in range from_block..min(BlockId::Latest..to_block)
					let limit = filter.limit;
					Either::B(Either::A(self.logs(filter)
						.map(move |mut logs| { logs.extend(pending); logs }) // append fetched pending logs
						.map(move |logs| limit_logs(logs, limit)) // limit the logs
						.map(FilterChanges::Logs)))
				},
                PollFilter::ReturnData(ref mut block_number) => {
                    // +1, cause we want to return hashes including current block hash.
                    let current_number = self.best_block_number() + 1;
                    let executed: Vec<(BlockId, Box<Vec<Bytes>>)> = (*block_number..current_number)
                        .filter_map(|block| {
                            let block_id = BlockId::Number(block);
                            let replay_result: Result<result::Result<Box<Iterator<Item = Executed>>, CallError>> = self.replay_block_transactions(block_id);
                            match replay_result {
                                Ok(Ok(executeds)) => {
                                    let output_bytes: Box<Vec<Bytes>> = Box::new(executeds
                                                                                 .map(|executed| executed.output)
                                                                                 .map(Bytes::from)
                                                                                 .collect()
                                                                                );
                                    Some((block_id, output_bytes))
                                },
                                Ok(Err(e)) => {
                                    warn!("Error replaying transactions for block {:?}: {:?}", block_id, e);
                                    None
                                },
                                Err(e) => {
                                    warn!("Error replaying transactions for block {:?}: {:?}", block_id, e);
                                    None
                                },
                            }
                        })
                    .collect();
                    let return_data = executed
                        .into_iter()
                        .map(|(block_id, output_bytes)| {
                            self.block_body(block_id)
                                .map(|body| {
                                    match body {
                                        None => vec![],
                                        Some(body) => {
                                            output_bytes
                                                .into_iter()
                                                .zip(body.transaction_hashes())
                                                .map(|(output_bytes, transaction_hash)| {
                                                    ReturnData {
                                                        transaction_hash,
                                                        return_data: output_bytes,
                                                        removed: false
                                                    }
                                                })
                                            .collect()
                                        },
                                    }
                                })
                        });
                    

                    *block_number = current_number;
                    Either::B(Either::B(join_all(return_data)
                                        .map(|return_data: Vec<Vec<ReturnData>>| {
                                            let return_data = return_data
                                                .into_iter()
                                                .flat_map(|rd| rd)
                                                .collect::<Vec<ReturnData>>();
                                            FilterChanges::ReturnData(return_data)
                                        })
                                       )
                             )
                }
            }
		})
	}

	fn filter_logs(&self, index: Index) -> BoxFuture<Vec<Log>> {
		let filter = {
			let mut polls = self.polls().lock();

			match polls.poll(&index.value()) {
				Some(&PollFilter::Logs(ref _block_number, ref _previous_log, ref filter)) => filter.clone(),
				// just empty array
				Some(_) => return Box::new(future::ok(Vec::new())),
				None => return Box::new(future::err(errors::filter_not_found())),
			}
		};

		let include_pending = filter.to_block == Some(BlockNumber::Pending);
		let filter: EthcoreFilter = filter.into();

		// fetch pending logs.
		let pending = if include_pending {
			let best_block = self.best_block_number();
			self.pending_logs(best_block, &filter)
		} else {
			Vec::new()
		};

		// retrieve logs asynchronously, appending pending logs.
		let limit = filter.limit;
		let logs = self.logs(filter);
		Box::new(logs
			.map(move |mut logs| { logs.extend(pending); logs })
			.map(move |logs| limit_logs(logs, limit))
		)
	}

	fn uninstall_filter(&self, index: Index) -> Result<bool> {
		Ok(self.polls().lock().remove_poll(&index.value()))
	}
}
