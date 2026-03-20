// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! # Pallet Coretime Market
//!
//! Implements RFC-17: Coretime Market Redesign.
//!
//! This pallet provides the market logic for bulk coretime sales using a clearing-price
//! descending Dutch auction model. It operates in three phases per sale cycle:
//!
//! 1. **Market Phase**: A descending clock auction where bidders place bids at or below
//!    the current descending price. Bids are binding and can only be raised, not cancelled.
//!
//! 2. **Renewal Phase**: Existing tenants with renewal rights can exercise them. If all
//!    cores are allocated from the auction, renewers may displace the lowest non-renewer
//!    auction winner. A penalty applies to renewers who did not participate in the auction
//!    when the market was oversubscribed.
//!
//! 3. **Settlement Phase**: No primary sales occur. The sale rotates to a new cycle once
//!    the relay chain has committed the timeslices covered by the current sale's regions.
//!
//! ## Design
//!
//! This pallet implements the [`Market`] and [`MarketState`] traits from `sp-coretime`,
//! allowing it to be used by the broker pallet without direct coupling. The broker calls
//! into the market trait for order placement and processes [`TickAction`]s returned by
//! [`Market::tick`] to perform fund transfers, region issuance, and sale rotation.
//!
//! Key design decisions:
//! - **Clearing-price auction**: All winners pay the same uniform price (the Kth highest bid).
//! - **Lock-then-charge**: Funds are locked at bid time by the broker. At settlement, excess
//!   is refunded via [`TickAction::Refund`]. Winners are charged the clearing price.
//! - **Binding bids**: Bids cannot be cancelled, only raised. This prevents gaming.
//! - **Displacement protection**: Auction winners with renewal rights cannot be displaced.

#![cfg_attr(not(feature = "std"), no_std)]

pub use pallet::*;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

extern crate alloc;

use alloc::{vec, vec::Vec};
use frame_support::{
	ensure,
	traits::{tokens::Balance as BalanceT, Get},
	weights::{Weight, WeightMeter},
};
use sp_arithmetic::FixedPointNumber;
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use scale_info::TypeInfo;
use sp_coretime::{
	AdaptPrice, AdaptedPrices, CloseBidResult, ConfigRecord, CoreCountProvider, CoreIndex,
	CoreMask, DisplacedBid, Market, MarketError, MarketState, OrderResult, PotentialRenewalId,
	RegionId, RenewalOrderResult, RenewalRightsProvider, SaleInfoRecord, SalePerformance,
	SalesStarted, StatusRecord, TickAction, Timeslice,
};
use sp_runtime::{
	traits::{AtLeast32BitUnsigned, SaturatedConversion, Saturating, Zero},
	BoundedVec, FixedPointOperand, FixedU64,
};

type BalanceOf<T> = <T as pallet::Config>::Balance;
type RelayBlockNumberOf<T> = <T as pallet::Config>::RelayBlockNumber;
type ConfigRecordOf<T> = ConfigRecord<RelayBlockNumberOf<T>>;
type SaleInfoRecordOf<T> = SaleInfoRecord<BalanceOf<T>, RelayBlockNumberOf<T>>;
type TickActionOf<T> =
	TickAction<BalanceOf<T>, <T as frame_system::Config>::AccountId, u32, SaleInfoRecordOf<T>>;

/// The phase of a Bulk Coretime Sale.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	Copy,
	Clone,
	PartialEq,
	Eq,
	Debug,
	TypeInfo,
	MaxEncodedLen,
)]
pub enum SalePhase {
	/// Market period: descending Dutch auction, bids accepted.
	Market,
	/// Renewal period: existing tenants can exercise renewal rights.
	Renewal,
	/// Settlement period: secondary market trading only, no primary sales.
	Settlement,
}

/// A bid in the descending clock auction.
#[derive(
	codec::Encode,
	codec::Decode,
	codec::DecodeWithMemTracking,
	Clone,
	PartialEq,
	Eq,
	Debug,
	scale_info::TypeInfo,
	codec::MaxEncodedLen,
)]
pub struct BidRecord<AccountId, Balance> {
	/// The bidder's account.
	pub who: AccountId,
	/// The bid price (amount locked from the bidder).
	pub price: Balance,
}

/// Record of an auction winner after settlement.
#[derive(
	codec::Encode,
	codec::Decode,
	codec::DecodeWithMemTracking,
	Clone,
	PartialEq,
	Eq,
	Debug,
	scale_info::TypeInfo,
	codec::MaxEncodedLen,
)]
pub struct AllocationRecord<AccountId, Balance> {
	/// The winning bidder.
	pub who: AccountId,
	/// The uniform clearing price charged to this winner.
	pub clearing_price: Balance,
	/// The original bid price (used for displacement priority — lowest bid displaced first).
	pub bid_price: Balance,
	/// The unique bid ID.
	pub bid_id: u32,
	/// The core index assigned to this allocation.
	pub core: CoreIndex,
	/// Whether this winner holds renewal rights and is protected from displacement.
	pub has_renewal_rights: bool,
}

/// Weight functions needed by the market pallet.
pub trait WeightInfo {
	fn place_order() -> Weight;
	fn raise_bid() -> Weight;
	fn place_renewal_order_market() -> Weight;
	fn place_renewal_order_renewal() -> Weight;
	fn place_renewal_order_displacement() -> Weight;
	fn settle_auction() -> Weight;
	fn finalize_sale() -> Weight;
	fn rotate_sale() -> Weight;
}

impl WeightInfo for () {
	fn place_order() -> Weight {
		Weight::zero()
	}
	fn raise_bid() -> Weight {
		Weight::zero()
	}
	fn place_renewal_order_market() -> Weight {
		Weight::zero()
	}
	fn place_renewal_order_renewal() -> Weight {
		Weight::zero()
	}
	fn place_renewal_order_displacement() -> Weight {
		Weight::zero()
	}
	fn settle_auction() -> Weight {
		Weight::zero()
	}
	fn finalize_sale() -> Weight {
		Weight::zero()
	}
	fn rotate_sale() -> Weight {
		Weight::zero()
	}
}

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::pallet_prelude::*;

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// Balance type used for bid amounts and prices.
		type Balance: BalanceT + FixedPointOperand;

		/// Relay chain block number type.
		type RelayBlockNumber: Parameter
			+ MaxEncodedLen
			+ AtLeast32BitUnsigned
			+ FixedPointOperand
			+ Copy;

		/// Weight information for market operations.
		type WeightInfo: WeightInfo;

		/// Price adaptation algorithm used when rotating sales.
		type PriceAdapter: AdaptPrice<Self::Balance>;

		/// Provider of the reserved core count (reservations + leases).
		type CoreCountProvider: CoreCountProvider;

		/// Provider of renewal rights information from the broker pallet.
		type RenewalRights: RenewalRightsProvider<Self::AccountId>;

		/// The number of relay chain blocks in a timeslice.
		#[pallet::constant]
		type TimeslicePeriod: Get<Self::RelayBlockNumber>;

		/// Maximum number of bids that can be placed in a single sale.
		#[pallet::constant]
		type MaxBids: Get<u32>;

		/// Price multiplier for the opening price of the descending auction.
		/// Opening price = previous clearing price * multiplier.
		#[pallet::constant]
		type PriceMultiplier: Get<u32>;
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// A new bid was placed during the Market phase.
		BidPlaced {
			/// The bidder.
			who: T::AccountId,
			/// Unique identifier for this bid.
			bid_id: u32,
			/// The bid amount locked from the bidder.
			amount: BalanceOf<T>,
		},
		/// An existing bid was raised to a higher price.
		BidRaised {
			/// The bidder.
			who: T::AccountId,
			/// Unique identifier of the bid.
			bid_id: u32,
			/// The new (higher) bid price.
			new_price: BalanceOf<T>,
			/// The additional amount that needs to be locked.
			additional: BalanceOf<T>,
		},
		/// The Market phase auction has been settled with a clearing price.
		AuctionSettled {
			/// The uniform clearing price that all winners pay.
			clearing_price: BalanceOf<T>,
			/// Number of auction winners.
			winners: u32,
		},
		/// Regions have been issued to auction winners at the end of the Renewal phase.
		SaleFinalized {
			/// Number of regions issued to auction winners.
			regions_issued: u32,
		},
		/// A renewal right was exercised during the Renewal phase.
		RenewalExercised {
			/// The renewing account.
			who: T::AccountId,
			/// The price paid for renewal.
			price: BalanceOf<T>,
			/// The assigned region.
			region_id: RegionId,
		},
		/// An auction winner was displaced by a renewer during the Renewal phase.
		BidDisplaced {
			/// The displaced auction winner.
			who: T::AccountId,
			/// The displaced bid ID.
			bid_id: u32,
			/// The amount to be refunded to the displaced winner.
			refund: BalanceOf<T>,
		},
		/// The sale phase has changed.
		PhaseTransitioned {
			/// The previous phase.
			from: SalePhase,
			/// The new phase.
			to: SalePhase,
		},
		/// A new sale has been initialized.
		SaleInitialized {
			/// The relay block number at which the sale starts.
			sale_start: RelayBlockNumberOf<T>,
			/// The length in relay chain blocks of the Market Period.
			market_period: RelayBlockNumberOf<T>,
			/// The price of Bulk Coretime at the beginning of the Market period.
			start_price: BalanceOf<T>,
			/// The reserve (floor) price of the descending auction.
			reserve_price: BalanceOf<T>,
			/// The first timeslice of the Regions being sold.
			region_begin: Timeslice,
			/// The timeslice on which the Regions being sold terminate.
			region_end: Timeslice,
			/// The number of cores we want to sell, ideally.
			ideal_cores_sold: CoreIndex,
			/// Number of cores which are/have been offered for sale.
			cores_offered: CoreIndex,
		},
	}

	/// The market configuration. Set by the broker via [`MarketState::set_configuration`].
	#[pallet::storage]
	pub type Configuration<T> = StorageValue<_, ConfigRecordOf<T>, OptionQuery>;

	/// The coretime sale status. Updated by the broker via [`MarketState::set_status`].
	#[pallet::storage]
	pub type Status<T> = StorageValue<_, StatusRecord, OptionQuery>;

	/// Information about the current sale.
	#[pallet::storage]
	pub type SaleInfo<T> = StorageValue<_, SaleInfoRecordOf<T>, OptionQuery>;

	/// The current phase of the sale cycle. `None` before sales are started.
	#[pallet::storage]
	pub type CurrentPhase<T> = StorageValue<_, SalePhase, OptionQuery>;

	/// Active bids during the Market phase. Keyed by bid ID.
	#[pallet::storage]
	pub type Bids<T: Config> = StorageMap<
		_,
		Blake2_128Concat,
		u32,
		BidRecord<T::AccountId, BalanceOf<T>>,
		OptionQuery,
	>;

	/// The next bid ID to assign. Also serves as the count of bids placed in this sale.
	#[pallet::storage]
	pub type NextBidId<T> = StorageValue<_, u32, ValueQuery>;

	/// Auction winners after settlement, awaiting region issuance at the end of Renewal phase.
	#[pallet::storage]
	pub type Allocations<T: Config> = StorageValue<
		_,
		BoundedVec<AllocationRecord<T::AccountId, BalanceOf<T>>, T::MaxBids>,
		ValueQuery,
	>;

	/// The clearing price from the most recent auction settlement.
	#[pallet::storage]
	pub type AuctionClearingPrice<T: Config> = StorageValue<_, BalanceOf<T>, OptionQuery>;

	/// Number of renewals exercised in the current Renewal phase.
	#[pallet::storage]
	pub type RenewalCount<T> = StorageValue<_, u32, ValueQuery>;
}

impl<T: Config> Market for Pallet<T> {
	type AccountId = T::AccountId;
	type Balance = BalanceOf<T>;
	type BlockNumber = RelayBlockNumberOf<T>;
	type Error = MarketError;
	type BidId = u32;
	type CoreCount = T::CoreCountProvider;
	type Config = ConfigRecordOf<T>;
	type SaleInfo = SaleInfoRecordOf<T>;

	fn start_sales(
		block_number: RelayBlockNumberOf<T>,
		reserve_price: BalanceOf<T>,
		core_count: CoreIndex,
	) -> Result<SalesStarted<BalanceOf<T>, Self::SaleInfo>, Self::Error> {
		let config = Configuration::<T>::get().ok_or(MarketError::Uninitialized)?;

		let commit_timeslice = latest_timeslice_ready_to_commit::<T>(block_number, &config);
		let status = StatusRecord {
			core_count,
			private_pool_size: 0,
			system_pool_size: 0,
			last_committed_timeslice: commit_timeslice.saturating_sub(1),
			last_timeslice: current_timeslice::<T>(block_number),
		};

		// Bootstrap with an imaginary previous sale.
		let old_sale = SaleInfoRecord {
			sale_start: block_number,
			opening_price: reserve_price,
			reserve_price,
			clearing_price: None,
			region_begin: commit_timeslice,
			region_end: commit_timeslice.saturating_add(config.region_length),
			first_core: 0,
			ideal_cores_sold: 0,
			cores_offered: 0,
			cores_sold: 0,
		};

		let reserved_cores = Self::CoreCount::reserved_core_count();
		let (new_prices, new_sale) =
			rotate_sale::<T>(&old_sale, &config, &status, reserved_cores, block_number);

		SaleInfo::<T>::put(&new_sale);
		Status::<T>::put(&status);
		CurrentPhase::<T>::put(SalePhase::Market);

		let start_price = new_sale.opening_price;
		Self::deposit_event(Event::SaleInitialized {
			sale_start: new_sale.sale_start,
			market_period: config.market_period,
			start_price,
			reserve_price: new_sale.reserve_price,
			region_begin: new_sale.region_begin,
			region_end: new_sale.region_end,
			ideal_cores_sold: new_sale.ideal_cores_sold,
			cores_offered: new_sale.cores_offered,
		});

		Ok(SalesStarted { old_sale, new_sale, new_prices, start_price })
	}

	fn place_order(
		block_number: RelayBlockNumberOf<T>,
		who: &T::AccountId,
		price_limit: BalanceOf<T>,
	) -> Result<OrderResult<Self::Balance, Self::BidId>, Self::Error> {
		ensure!(CurrentPhase::<T>::get() == Some(SalePhase::Market), MarketError::WrongPhase);
		let sale = SaleInfo::<T>::get().ok_or(MarketError::NoSales)?;
		ensure!(block_number >= sale.sale_start, MarketError::TooEarly);

		let bid_count = NextBidId::<T>::get();
		ensure!(bid_count < T::MaxBids::get(), MarketError::SoldOut);

		let current_price = descending_price::<T>(block_number, &sale);
		let bid_price = price_limit.min(current_price);

		let bid_id = bid_count;
		NextBidId::<T>::put(bid_id.saturating_add(1));

		Bids::<T>::insert(bid_id, BidRecord { who: who.clone(), price: bid_price });

		Self::deposit_event(Event::BidPlaced {
			who: who.clone(),
			bid_id,
			amount: bid_price,
		});

		Ok(OrderResult::BidPlaced { id: bid_id, bid_price })
	}

	fn place_renewal_order(
		block_number: RelayBlockNumberOf<T>,
		who: &T::AccountId,
		_renewal: PotentialRenewalId,
		recorded_price: BalanceOf<T>,
	) -> Result<RenewalOrderResult<Self::Balance, Self::BidId, Self::AccountId>, Self::Error> {
		let config = Configuration::<T>::get().ok_or(MarketError::Uninitialized)?;
		let status = Status::<T>::get().ok_or(MarketError::Uninitialized)?;
		let sale = SaleInfo::<T>::get().ok_or(MarketError::NoSales)?;

		match CurrentPhase::<T>::get().ok_or(MarketError::Uninitialized)? {
			SalePhase::Renewal => {
				let clearing =
					AuctionClearingPrice::<T>::get().unwrap_or(sale.reserve_price);

				// Penalty applies when market was oversubscribed (all cores filled in auction).
				let allocations = Allocations::<T>::get();
				let oversubscribed = allocations.len() as u16 >= sale.cores_offered;

				let penalty = if oversubscribed {
					config.penalty * clearing
				} else {
					Zero::zero()
				};
				let renewal_price = clearing.saturating_add(penalty);

				let allocated_count =
					allocations.len() as u16 + RenewalCount::<T>::get() as u16;
				let available = status.core_count.saturating_sub(sale.first_core);

				if allocated_count < available {
					// Unallocated core available — direct sale.
					let core = sale.first_core.saturating_add(allocated_count);
					let region_id = RegionId {
						begin: sale.region_begin,
						core,
						mask: CoreMask::complete(),
					};
					RenewalCount::<T>::mutate(|c| c.saturating_inc());

					Self::deposit_event(Event::RenewalExercised {
						who: who.clone(),
						price: renewal_price,
						region_id,
					});

					Ok(RenewalOrderResult::Sold {
						price: renewal_price,
						next_renewal_price: renewal_price,
						region_id,
						effective_to: sale.region_end,
						displaced: None,
					})
				} else if oversubscribed {
					// All cores allocated — displace lowest non-renewer auction winner.
					let mut allocs = Allocations::<T>::get();

					let displace_idx = allocs
						.iter()
						.enumerate()
						.filter(|(_, a)| !a.has_renewal_rights)
						.min_by_key(|(_, a)| a.bid_price)
						.map(|(i, _)| i);

					if let Some(idx) = displace_idx {
						let displaced_alloc = allocs.remove(idx);

						// Renewer gets the displaced winner's core.
						let region_id = RegionId {
							begin: sale.region_begin,
							core: displaced_alloc.core,
							mask: CoreMask::complete(),
						};

						// Displaced winner gets clearing_price refunded (excess was already
						// refunded during settlement).
						let refund = displaced_alloc.clearing_price;

						Self::deposit_event(Event::BidDisplaced {
							who: displaced_alloc.who.clone(),
							bid_id: displaced_alloc.bid_id,
							refund,
						});

						Allocations::<T>::put(allocs);
						RenewalCount::<T>::mutate(|c| c.saturating_inc());

						Self::deposit_event(Event::RenewalExercised {
							who: who.clone(),
							price: renewal_price,
							region_id,
						});

						Ok(RenewalOrderResult::Sold {
							price: renewal_price,
							next_renewal_price: renewal_price,
							region_id,
							effective_to: sale.region_end,
							displaced: Some(DisplacedBid {
								who: displaced_alloc.who,
								refund,
								bid_id: displaced_alloc.bid_id,
							}),
						})
					} else {
						// All remaining winners have renewal rights — cannot displace.
						Err(MarketError::Unavailable)
					}
				} else {
					Err(MarketError::Unavailable)
				}
			},
			SalePhase::Market | SalePhase::Settlement => Err(MarketError::WrongPhase),
		}
	}

	fn raise_bid(
		block_number: RelayBlockNumberOf<T>,
		id: Self::BidId,
		who: &T::AccountId,
		new_price: BalanceOf<T>,
	) -> Result<BalanceOf<T>, Self::Error> {
		ensure!(CurrentPhase::<T>::get() == Some(SalePhase::Market), MarketError::WrongPhase);
		let sale = SaleInfo::<T>::get().ok_or(MarketError::NoSales)?;

		let mut bid = Bids::<T>::get(id).ok_or(MarketError::BidNotExist)?;
		ensure!(&bid.who == who, MarketError::BidNotExist);
		ensure!(new_price > bid.price, MarketError::Overpriced);

		// New price must not exceed the current descending price.
		let current_price = descending_price::<T>(block_number, &sale);
		ensure!(new_price <= current_price, MarketError::BidTooHigh);

		let additional = new_price.saturating_sub(bid.price);
		bid.price = new_price;
		Bids::<T>::insert(id, bid);

		Self::deposit_event(Event::BidRaised {
			who: who.clone(),
			bid_id: id,
			new_price,
			additional,
		});

		Ok(additional)
	}

	fn close_bid(
		_id: Self::BidId,
		_maybe_check_owner: Option<T::AccountId>,
	) -> Result<CloseBidResult<T::AccountId, BalanceOf<T>>, Self::Error> {
		// RFC-17: bids are binding and cannot be cancelled.
		Err(MarketError::BidNotCancellable)
	}

	fn tick(
		block_number: RelayBlockNumberOf<T>,
		weight_meter: &mut WeightMeter,
	) -> Vec<TickActionOf<T>> {
		let (Some(config), Some(status)) = (Configuration::<T>::get(), Status::<T>::get()) else {
			return vec![];
		};
		let Some(sale) = SaleInfo::<T>::get() else {
			return vec![];
		};

		let Some(phase) = CurrentPhase::<T>::get() else {
			return vec![];
		};

		match phase {
			SalePhase::Market => {
				let market_end = sale.sale_start.saturating_add(config.market_period);

				if block_number >= market_end {
					if !weight_meter.can_consume(T::WeightInfo::settle_auction()) {
						return vec![];
					}
					weight_meter.consume(T::WeightInfo::settle_auction());

					let mut actions = settle_auction::<T>(&sale);
					CurrentPhase::<T>::put(SalePhase::Renewal);

					Self::deposit_event(Event::PhaseTransitioned {
						from: SalePhase::Market,
						to: SalePhase::Renewal,
					});

					actions.push(TickAction::ProcessRenewals);
					return actions;
				}
			},
			SalePhase::Renewal => {
				let market_end = sale.sale_start.saturating_add(config.market_period);
				let renewal_end = market_end.saturating_add(config.renewal_period);

				if block_number >= renewal_end {
					if !weight_meter.can_consume(T::WeightInfo::finalize_sale()) {
						return vec![];
					}
					weight_meter.consume(T::WeightInfo::finalize_sale());

					let actions = finalize_sale::<T>(&sale);
					CurrentPhase::<T>::put(SalePhase::Settlement);
					RenewalCount::<T>::kill();

					Self::deposit_event(Event::PhaseTransitioned {
						from: SalePhase::Renewal,
						to: SalePhase::Settlement,
					});

					return actions;
				}
			},
			SalePhase::Settlement => {
				if status.last_committed_timeslice >= sale.region_begin {
					if !weight_meter.can_consume(T::WeightInfo::rotate_sale()) {
						return vec![];
					}
					weight_meter.consume(T::WeightInfo::rotate_sale());

					let reserved_cores = Self::CoreCount::reserved_core_count();
					let (new_prices, new_sale) = rotate_sale::<T>(
						&sale,
						&config,
						&status,
						reserved_cores,
						block_number,
					);

					SaleInfo::<T>::put(&new_sale);
					CurrentPhase::<T>::put(SalePhase::Market);

					// Clean up state from previous sale.
					NextBidId::<T>::kill();
					AuctionClearingPrice::<T>::kill();

					Self::deposit_event(Event::PhaseTransitioned {
						from: SalePhase::Settlement,
						to: SalePhase::Market,
					});
					let start_price = new_sale.opening_price;
					Self::deposit_event(Event::SaleInitialized {
						sale_start: new_sale.sale_start,
						market_period: config.market_period,
						start_price,
						reserve_price: new_sale.reserve_price,
						region_begin: new_sale.region_begin,
						region_end: new_sale.region_end,
						ideal_cores_sold: new_sale.ideal_cores_sold,
						cores_offered: new_sale.cores_offered,
					});
					return vec![TickAction::SaleRotated {
						old_sale: sale,
						new_sale,
						new_prices,
						start_price,
					}];
				}
			},
		}

		vec![]
	}
}

impl<T: Config> MarketState for Pallet<T> {
	fn configuration() -> Option<ConfigRecordOf<T>> {
		Configuration::<T>::get()
	}

	fn set_configuration(config: ConfigRecordOf<T>) {
		Configuration::<T>::put(config);
	}

	fn status() -> Option<StatusRecord> {
		Status::<T>::get()
	}

	fn set_status(status: StatusRecord) {
		Status::<T>::put(status);
	}

	fn sale_info() -> Option<SaleInfoRecordOf<T>> {
		SaleInfo::<T>::get()
	}

	fn set_sale_info(sale_info: SaleInfoRecordOf<T>) {
		SaleInfo::<T>::put(sale_info);
	}

	fn current_price(block_number: RelayBlockNumberOf<T>) -> Option<BalanceOf<T>> {
		let sale = SaleInfo::<T>::get()?;
		match CurrentPhase::<T>::get()? {
			SalePhase::Market => Some(descending_price::<T>(block_number, &sale)),
			SalePhase::Renewal | SalePhase::Settlement => AuctionClearingPrice::<T>::get(),
		}
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn benchmark_config() -> Self::Config {
		use sp_arithmetic::Perbill;
		ConfigRecord {
			advance_notice: 2u32.into(),
			market_period: 1u32.into(),
			renewal_period: 1u32.into(),
			ideal_bulk_proportion: Default::default(),
			limit_cores_offered: None,
			region_length: 3,
			penalty: Perbill::from_percent(10),
			contribution_timeout: 5,
		}
	}
}

// ---------------------------------------------------------------------------
// Internal functions
// ---------------------------------------------------------------------------

/// Compute the descending price at the given block during the Market phase.
///
/// The price descends linearly from `opening_price` to `reserve_price` over the
/// configured `market_period`.
fn descending_price<T: Config>(
	now: RelayBlockNumberOf<T>,
	sale: &SaleInfoRecordOf<T>,
) -> BalanceOf<T> {
	let config = Configuration::<T>::get();
	let market_period = config.map(|c| c.market_period).unwrap_or_else(|| now);

	let elapsed = now.saturating_sub(sale.sale_start).min(market_period);
	if market_period.is_zero() {
		return sale.reserve_price;
	}

	let price_range = sale.opening_price.saturating_sub(sale.reserve_price);
	let elapsed_u128: u128 = elapsed.saturated_into();
	let period_u128: u128 = market_period.saturated_into();
	let descent =
		FixedU64::from_rational(elapsed_u128, period_u128).saturating_mul_int(price_range);

	sale.opening_price.saturating_sub(descent)
}

/// Settle the auction at the end of the Market phase.
///
/// Sorts all bids by price descending, determines the clearing price (Kth highest bid,
/// floored at reserve price), creates allocations for winners, and generates refund
/// actions for losers and excess amounts.
fn settle_auction<T: Config>(sale: &SaleInfoRecordOf<T>) -> Vec<TickActionOf<T>> {
	let mut actions = vec![];

	// Collect and sort all bids by price descending.
	let mut all_bids: Vec<(u32, BidRecord<T::AccountId, BalanceOf<T>>)> = Vec::new();
	for (id, bid) in Bids::<T>::iter() {
		all_bids.push((id, bid));
	}
	all_bids.sort_by(|a, b| b.1.price.cmp(&a.1.price));

	let k = sale.cores_offered as usize;
	let reserve = sale.reserve_price;

	// Clearing price: Kth highest bid, floored at reserve price.
	let clearing_price = if all_bids.len() >= k && k > 0 {
		all_bids[k - 1].1.price.max(reserve)
	} else {
		reserve
	};

	AuctionClearingPrice::<T>::put(clearing_price);

	let mut allocations: Vec<AllocationRecord<T::AccountId, BalanceOf<T>>> = Vec::new();
	let mut winner_count = 0u32;

	for (i, (bid_id, bid)) in all_bids.into_iter().enumerate() {
		Bids::<T>::remove(bid_id);

		if i < k && bid.price >= clearing_price {
			// Winner: refund excess (bid_price - clearing_price).
			let excess = bid.price.saturating_sub(clearing_price);
			if !excess.is_zero() {
				actions
					.push(TickAction::Refund { amount: excess, who: bid.who.clone() });
			}

			let has_renewal_rights =
				T::RenewalRights::renewal_rights_count(&bid.who, sale.region_end) > 0;

			let core = sale.first_core.saturating_add(i as u16);

			allocations.push(AllocationRecord {
				who: bid.who,
				clearing_price,
				bid_price: bid.price,
				bid_id,
				core,
				has_renewal_rights,
			});

			winner_count += 1;
		} else {
			// Loser: full refund.
			actions.push(TickAction::Refund { amount: bid.price, who: bid.who });
		}
	}

	let bounded: BoundedVec<_, T::MaxBids> = BoundedVec::truncate_from(allocations);
	Allocations::<T>::put(bounded);

	// Update sale info with results.
	let mut updated_sale = sale.clone();
	updated_sale.cores_sold = winner_count as u16;
	updated_sale.clearing_price = Some(clearing_price);
	SaleInfo::<T>::put(updated_sale);

	Pallet::<T>::deposit_event(Event::AuctionSettled { clearing_price, winners: winner_count });

	actions
}

/// Finalize the sale at the end of the Renewal phase.
///
/// Issues regions for all remaining auction allocations (those not displaced by renewers).
fn finalize_sale<T: Config>(sale: &SaleInfoRecordOf<T>) -> Vec<TickActionOf<T>> {
	let mut actions = vec![];
	let allocations = Allocations::<T>::take();
	let count = allocations.len() as u32;

	for alloc in allocations.into_iter() {
		let region_id = RegionId {
			begin: sale.region_begin,
			core: alloc.core,
			mask: CoreMask::complete(),
		};

		actions.push(TickAction::SellRegion {
			owner: alloc.who,
			paid: alloc.clearing_price,
			region_id,
			region_end: sale.region_end,
		});
	}

	Pallet::<T>::deposit_event(Event::SaleFinalized { regions_issued: count });

	actions
}

fn current_timeslice<T: Config>(now: RelayBlockNumberOf<T>) -> Timeslice {
	let timeslice_period = T::TimeslicePeriod::get();
	(now / timeslice_period).saturated_into()
}

fn latest_timeslice_ready_to_commit<T: Config>(
	now: RelayBlockNumberOf<T>,
	config: &ConfigRecordOf<T>,
) -> Timeslice {
	let advanced = now.saturating_add(config.advance_notice);
	let timeslice_period = T::TimeslicePeriod::get();
	(advanced / timeslice_period).saturated_into()
}

fn adapt_prices<T: Config>(old_sale: &SaleInfoRecordOf<T>) -> AdaptedPrices<BalanceOf<T>> {
	T::PriceAdapter::adapt_price(SalePerformance::from_sale(old_sale))
}

/// Rotate to a new sale based on the previous sale's performance.
fn rotate_sale<T: Config>(
	old_sale: &SaleInfoRecordOf<T>,
	config: &ConfigRecordOf<T>,
	status: &StatusRecord,
	reserved_cores: CoreIndex,
	now: RelayBlockNumberOf<T>,
) -> (AdaptedPrices<BalanceOf<T>>, SaleInfoRecordOf<T>) {
	let new_prices = adapt_prices::<T>(old_sale);

	let max_possible_sales = status.core_count.saturating_sub(reserved_cores);
	let limit_cores_offered = config.limit_cores_offered.unwrap_or(CoreIndex::max_value());
	let cores_offered = limit_cores_offered.min(max_possible_sales);
	let ideal_cores_sold = (config.ideal_bulk_proportion * cores_offered as u32) as u16;

	let region_begin = old_sale.region_end;
	let region_end = region_begin + config.region_length;

	// Opening price based on previous clearing price.
	let previous_clearing = old_sale.clearing_price.unwrap_or(old_sale.reserve_price);
	let opening_price =
		previous_clearing.saturating_mul(T::PriceMultiplier::get().into());

	let new_sale = SaleInfoRecord {
		sale_start: now,
		opening_price,
		reserve_price: new_prices.reserve_price,
		clearing_price: None,
		region_begin,
		region_end,
		first_core: reserved_cores,
		ideal_cores_sold,
		cores_offered,
		cores_sold: 0,
	};

	(new_prices, new_sale)
}
