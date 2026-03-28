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
//! 3. **Settlement Phase**: No primary sales occur. The pallet waits until the next
//!    sale's region begins before rotating into a new market cycle. Regions are issued
//!    at the transition from Renewal to Settlement.
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
use sp_arithmetic::Perbill;
use sp_coretime::{
	AdaptedPrices, CoreCountProvider, CoreIndex, CoreMask, DisplacedBid, Market,
	MarketConfig, MarketError, MarketSaleInfo, MarketState, OrderResult, PotentialRenewalId,
	RegionId, RenewalOrderResult, RenewalRightsProvider, SalesStarted, TickAction,
	Timeslice,
};
use sp_runtime::{
	traits::{AtLeast32BitUnsigned, SaturatedConversion, Saturating, Zero},
	BoundedVec, FixedPointOperand, FixedU64,
};

/// The status of a Bulk Coretime Sale (RFC-17 model).
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen,
)]
pub struct SaleInfoRecord<Balance, BlockNumber> {
	/// The relay block number at which the sale (Market phase) starts.
	pub sale_start: BlockNumber,
	/// The opening price of the descending Dutch auction.
	pub opening_price: Balance,
	/// The reserve price (floor price of the descending auction).
	pub reserve_price: Balance,
	/// The clearing price (uniform price all winners pay). Set after auction settlement.
	pub clearing_price: Option<Balance>,
	/// The first timeslice of the Regions which are being sold in this sale.
	pub region_begin: Timeslice,
	/// The timeslice on which the Regions being sold in this sale expire.
	pub region_end: Timeslice,
	/// The number of cores we want to sell, ideally. Selling this amount would result in no
	/// change to the price for the next sale.
	pub ideal_cores_sold: CoreIndex,
	/// Number of cores offered for sale.
	pub cores_offered: CoreIndex,
	/// The index of the first core for sale. Sold regions are assigned core indices
	/// incrementing from this value.
	pub first_core: CoreIndex,
	/// Number of cores which have been sold; never more than cores_offered.
	pub cores_sold: CoreIndex,
}

impl<Balance: Clone, BlockNumber: Clone> MarketSaleInfo for SaleInfoRecord<Balance, BlockNumber> {
	type Balance = Balance;
	type BlockNumber = BlockNumber;

	fn sale_start(&self) -> BlockNumber {
		self.sale_start.clone()
	}
	fn region_begin(&self) -> Timeslice {
		self.region_begin
	}
	fn region_end(&self) -> Timeslice {
		self.region_end
	}
	fn ideal_cores_sold(&self) -> CoreIndex {
		self.ideal_cores_sold
	}
	fn cores_offered(&self) -> CoreIndex {
		self.cores_offered
	}
	fn first_core(&self) -> CoreIndex {
		self.first_core
	}
	fn cores_sold(&self) -> CoreIndex {
		self.cores_sold
	}
}

/// Configuration of the coretime system (RFC-17 model).
///
/// All governance-adjustable parameters from RFC-17 are stored here so they can be
/// updated at runtime via `set_configuration`.
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen,
)]
pub struct ConfigRecord<BlockNumber, Balance> {
	/// The number of Relay-chain blocks in advance which scheduling should be fixed and the
	/// `Coretime::assign` API used to inform the Relay-chain.
	pub advance_notice: BlockNumber,
	/// The length in blocks of the Market (auction) phase.
	pub market_period: BlockNumber,
	/// The length in blocks of the Renewal phase.
	pub renewal_period: BlockNumber,
	/// The length in timeslices of Regions which are up for sale in forthcoming sales.
	pub region_length: Timeslice,
	/// The proportion of cores available for sale which should be sold.
	pub ideal_bulk_proportion: Perbill,
	/// An artificial limit to the number of cores which are allowed to be sold. If `Some` then
	/// no more cores will be sold than this.
	pub limit_cores_offered: Option<CoreIndex>,
	/// Penalty applied to renewers who didn't win in the auction (when market is oversubscribed).
	/// RFC-17: e.g. 30%.
	pub penalty: Perbill,
	/// The duration by which rewards for contributions to the InstaPool must be collected.
	pub contribution_timeout: Timeslice,
	/// Multiplier applied to the reserve price to derive the opening price.
	/// RFC-17: recommended 3.
	pub price_multiplier: u32,
	/// Minimum opening price floor. RFC-17: recommended 150 DOT.
	pub min_opening_price: Balance,
	/// Target consumption rate for reserve price adjustment. RFC-17: recommended 90%.
	pub target_consumption_rate: Perbill,
	/// Sensitivity parameter (K) in milliunits. Divide by 1000 to get the actual K value.
	/// E.g. 2500 = K of 2.5. RFC-17: recommended 2000-3000 (i.e. K = 2-3).
	pub sensitivity_millis: u32,
	/// Minimum reserve price floor. RFC-17: recommended 1 DOT.
	pub min_reserve_price: Balance,
	/// Minimum absolute reserve price increase when consumption is 100%.
	/// RFC-17: recommended 100 DOT.
	pub min_increment: Balance,
}

impl<BlockNumber, Balance> ConfigRecord<BlockNumber, Balance>
where
	BlockNumber: sp_arithmetic::traits::Zero,
{
	/// Check the config for basic validity constraints.
	pub fn validate(&self) -> Result<(), ()> {
		if self.market_period.is_zero() {
			return Err(());
		}

		Ok(())
	}
}

impl<BlockNumber: Clone, Balance> MarketConfig for ConfigRecord<BlockNumber, Balance>
where
	BlockNumber: sp_arithmetic::traits::Zero,
{
	type BlockNumber = BlockNumber;

	fn advance_notice(&self) -> BlockNumber {
		self.advance_notice.clone()
	}
	fn region_length(&self) -> Timeslice {
		self.region_length
	}
	fn contribution_timeout(&self) -> Timeslice {
		self.contribution_timeout
	}
	fn validate(&self) -> Result<(), ()> {
		ConfigRecord::validate(self)
	}
}

type BalanceOf<T> = <T as pallet::Config>::Balance;
type RelayBlockNumberOf<T> = <T as pallet::Config>::RelayBlockNumber;
type ConfigRecordOf<T> = ConfigRecord<RelayBlockNumberOf<T>, BalanceOf<T>>;
type SaleInfoRecordOf<T> = SaleInfoRecord<BalanceOf<T>, RelayBlockNumberOf<T>>;
type TickActionOf<T> =
	TickAction<BalanceOf<T>, <T as frame_system::Config>::AccountId, SaleInfoRecordOf<T>>;

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
	/// Settlement period: no primary sales, awaiting next sale rotation.
	Settlement,
}

/// A bid in the descending clock auction.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	Clone,
	PartialEq,
	Eq,
	Debug,
	TypeInfo,
	MaxEncodedLen,
)]
pub struct BidRecord<AccountId, Balance> {
	/// The bidder's account.
	pub who: AccountId,
	/// The bid price (amount locked from the bidder).
	pub price: Balance,
}

/// Record of an auction winner after settlement.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	Clone,
	PartialEq,
	Eq,
	Debug,
	TypeInfo,
	MaxEncodedLen,
)]
pub struct AllocationRecord<AccountId, Balance> {
	/// The winning bidder.
	pub who: AccountId,
	/// The original bid price (used for displacement priority — lowest bid displaced first).
	pub bid_price: Balance,
	/// The unique bid ID.
	pub bid_id: u32,
	/// The core index assigned to this allocation.
	pub core: CoreIndex,
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
			/// Number of cores offered for sale.
			cores_offered: CoreIndex,
		},
	}

	/// The market configuration. Set by the broker via [`MarketState::set_configuration`].
	#[pallet::storage]
	pub type Configuration<T> = StorageValue<_, ConfigRecordOf<T>, OptionQuery>;

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
			rotate_sale::<T>(&old_sale, &config, core_count, reserved_cores, block_number);

		SaleInfo::<T>::put(&new_sale);
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
		ensure!(bid_count < T::MaxBids::get(), MarketError::TooManyBids);

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
		_block_number: RelayBlockNumberOf<T>,
		who: &T::AccountId,
		_renewal: PotentialRenewalId,
		_recorded_price: BalanceOf<T>,
		core_count: CoreIndex,
	) -> Result<RenewalOrderResult<Self::Balance, Self::BidId, Self::AccountId>, Self::Error> {
		let config = Configuration::<T>::get().ok_or(MarketError::Uninitialized)?;
		let sale = SaleInfo::<T>::get().ok_or(MarketError::NoSales)?;

		match CurrentPhase::<T>::get().ok_or(MarketError::Uninitialized)? {
			SalePhase::Renewal => {
				let clearing =
					AuctionClearingPrice::<T>::get().unwrap_or(sale.reserve_price);

				// Penalty applies when the auction filled all offered cores, signaling high
				// demand. This is checked once against auction outcomes (not total
				// demand including renewers) so that all renewers pay a uniform penalty.
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
				let available = core_count.saturating_sub(sale.first_core);

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
					let mut allocs = allocations;

					let displace_idx = allocs
						.iter()
						.enumerate()
						.filter(|(_, a)| {
							T::RenewalRights::renewal_rights_count(
								&a.who,
								sale.region_begin,
							) == 0
						})
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

						// Displaced winner gets clearing price refunded (excess was already
						// refunded during settlement).
						let refund = sale.clearing_price.unwrap_or_default();

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

	fn tick(
		block_number: RelayBlockNumberOf<T>,
		core_count: CoreIndex,
		last_committed_timeslice: Timeslice,
		weight_meter: &mut WeightMeter,
	) -> Vec<TickActionOf<T>> {
		let Some(config) = Configuration::<T>::get() else {
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
				if last_committed_timeslice >= sale.region_begin {
					if !weight_meter.can_consume(T::WeightInfo::rotate_sale()) {
						return vec![];
					}
					weight_meter.consume(T::WeightInfo::rotate_sale());

					let reserved_cores = Self::CoreCount::reserved_core_count();
					let (new_prices, new_sale) = rotate_sale::<T>(
						&sale,
						&config,
						core_count,
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
		ConfigRecord {
			advance_notice: 2u32.into(),
			market_period: 1u32.into(),
			renewal_period: 1u32.into(),
			ideal_bulk_proportion: Default::default(),
			limit_cores_offered: None,
			region_length: 3,
			penalty: Perbill::from_percent(10),
			contribution_timeout: 5,
			price_multiplier: 2,
			min_opening_price: 10u32.into(),
			target_consumption_rate: Perbill::from_percent(90),
			sensitivity_millis: 2500,
			min_reserve_price: 1u32.into(),
			min_increment: 100u32.into(),
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

/// Fisher-Yates shuffle of the sub-slice of bids that tie at the clearing price.
///
/// RFC-17 specifies random selection at the marginal step using the parent block hash as
/// entropy. After the initial sort by price descending, bids strictly above the clearing
/// price are already guaranteed winners and bids strictly below are guaranteed losers. Only
/// the bids exactly at `clearing_price` need to be shuffled so that winners among them are
/// chosen fairly.
fn shuffle_marginal_bids<T: Config>(
	bids: &mut [(u32, BidRecord<T::AccountId, BalanceOf<T>>)],
	clearing_price: BalanceOf<T>,
) {
	// Find the contiguous range of bids at the clearing price using binary search.
	// After the descending sort: [above, above, ..., AT, AT, AT, ..., below, below, ...]
	// partition_point finds the first element where the predicate is false.
	let start = bids.partition_point(|b| b.1.price > clearing_price);
	let end = bids.partition_point(|b| b.1.price >= clearing_price);

	if end.saturating_sub(start) <= 1 {
		// Zero or one bid at the clearing price — nothing to shuffle.
		return;
	}

	let slice = &mut bids[start..end];
	let n = slice.len();

	// Use parent block hash as entropy source for the shuffle.
	let seed = frame_system::Pallet::<T>::parent_hash();
	let seed_bytes: &[u8] = seed.as_ref();

	// Fisher-Yates shuffle (Durstenfeld variant): iterate from the end, swap each element
	// with a randomly chosen element from the remaining unshuffled portion.
	//
	// Each step consumes 4 bytes from the hash to produce a random index, wrapping around
	// if there are more steps than the hash can cover.
	let hash_len = seed_bytes.len().saturating_sub(3);
	if hash_len == 0 {
		return;
	}
	for i in (1..n).rev() {
		let offset = ((i - 1) * 4) % hash_len;
		let rand_val = u32::from_le_bytes(
			seed_bytes[offset..offset + 4]
				.try_into()
				.expect("offset + 4 is within bounds; qed"),
		);
		let j = (rand_val as usize) % (i + 1);
		slice.swap(i, j);
	}
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

	// RFC-17: Randomly shuffle bids that tie at the clearing price using the parent block
	// hash as entropy. This ensures fair selection when not all bids at the marginal price
	// can win.
	shuffle_marginal_bids::<T>(&mut all_bids, clearing_price);

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

			let core = sale.first_core.saturating_add(i as u16);

			allocations.push(AllocationRecord {
				who: bid.who,
				bid_price: bid.price,
				bid_id,
				core,
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
	let clearing_price = sale.clearing_price.unwrap_or(sale.reserve_price);

	for alloc in allocations.into_iter() {
		let region_id = RegionId {
			begin: sale.region_begin,
			core: alloc.core,
			mask: CoreMask::complete(),
		};

		actions.push(TickAction::SellRegion {
			owner: alloc.who,
			paid: clearing_price,
			region_id,
			region_end: sale.region_end,
		});
	}

	// Update cores_sold to include renewals so that adjust_reserve_price uses the total
	// consumption (auction winners + renewals) for the next sale's reserve price.
	let renewal_count = RenewalCount::<T>::get() as u16;
	if renewal_count > 0 {
		let mut updated_sale = sale.clone();
		updated_sale.cores_sold = updated_sale.cores_sold.saturating_add(renewal_count);
		SaleInfo::<T>::put(updated_sale);
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

/// Compute the new reserve price per RFC-17's exponential adjustment:
///
/// `price_candidate = reserve_price * exp(K * (consumption_rate - TARGET))`
/// `price_candidate = max(price_candidate, P_MIN)`
/// If consumption == 100% and increase < MIN_INCREMENT: use reserve + MIN_INCREMENT instead.
fn adjust_reserve_price<T: Config>(
	old_sale: &SaleInfoRecordOf<T>,
	config: &ConfigRecordOf<T>,
) -> BalanceOf<T> {
	let cores_offered = old_sale.cores_offered;
	if cores_offered == 0 {
		// Bootstrap: no previous sale data, keep the initial reserve price.
		return old_sale.reserve_price;
	}

	// consumption_rate = cores_sold / cores_offered (including renewals).
	let consumption_rate =
		Perbill::from_rational(old_sale.cores_sold as u32, cores_offered as u32);
	let target = config.target_consumption_rate;

	let k = FixedU64::from_rational(config.sensitivity_millis as u128, 1000);

	// (consumption_rate - target) can be negative; compute absolute value and sign.
	let (deviation, positive) = if consumption_rate >= target {
		(consumption_rate - target, true)
	} else {
		(target - consumption_rate, false)
	};

	// deviation as FixedU64 (Perbill is [0,1], convert via its inner value).
	let dev = FixedU64::from_rational(deviation.deconstruct() as u128, 1_000_000_000);
	let exponent = k.saturating_mul(dev);

	// Compute exp(x) via Taylor series: sum(x^k / k!) for k = 0, 1, 2, ...
	// Iterates until terms become negligible (< 1e-12 in FixedU64).
	// For exp(-x), compute exp(x) and invert: exp(-x) = 1 / exp(x).
	let x = exponent;
	let mut sum = FixedU64::from(1);
	let mut term = FixedU64::from(1);
	for n in 1..30u64 {
		term = term.saturating_mul(x) / FixedU64::saturating_from_integer(n);
		if term.into_inner() == 0 {
			break;
		}
		sum = sum.saturating_add(term);
	}
	let exp_approx = if positive {
		sum
	} else {
		FixedU64::saturating_from_rational(FixedU64::from(1).into_inner(), sum.into_inner())
	};

	let mut price_candidate = exp_approx.saturating_mul_int(old_sale.reserve_price);

	// Floor at P_MIN.
	if price_candidate < config.min_reserve_price {
		price_candidate = config.min_reserve_price;
	}

	// If 100% consumption and increase < MIN_INCREMENT, apply MIN_INCREMENT instead.
	if consumption_rate == Perbill::one() {
		let increase = price_candidate.saturating_sub(old_sale.reserve_price);
		if increase < config.min_increment {
			price_candidate = old_sale.reserve_price.saturating_add(config.min_increment);
		}
	}

	price_candidate
}

/// Rotate to a new sale based on the previous sale's performance.
fn rotate_sale<T: Config>(
	old_sale: &SaleInfoRecordOf<T>,
	config: &ConfigRecordOf<T>,
	core_count: CoreIndex,
	reserved_cores: CoreIndex,
	now: RelayBlockNumberOf<T>,
) -> (AdaptedPrices<BalanceOf<T>>, SaleInfoRecordOf<T>) {
	let new_reserve = adjust_reserve_price::<T>(old_sale, config);
	let new_prices = AdaptedPrices {
		reserve_price: new_reserve,
		target_price: old_sale.clearing_price.unwrap_or(new_reserve),
	};

	let max_possible_sales = core_count.saturating_sub(reserved_cores);
	let limit_cores_offered = config.limit_cores_offered.unwrap_or(CoreIndex::max_value());
	let cores_offered = limit_cores_offered.min(max_possible_sales);
	let ideal_cores_sold = (config.ideal_bulk_proportion * cores_offered as u32) as u16;

	let region_begin = old_sale.region_end;
	let region_end = region_begin + config.region_length;

	// RFC-17: opening_price = max(min_opening_price, price_multiplier * reserve_price).
	let opening_price = new_prices
		.reserve_price
		.saturating_mul(config.price_multiplier.into())
		.max(config.min_opening_price);

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
