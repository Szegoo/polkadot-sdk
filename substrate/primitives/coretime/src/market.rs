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

use crate::SalePhase;
use alloc::vec::Vec;
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use scale_info::TypeInfo;
use sp_runtime::DispatchError;
use sp_weights::WeightMeter;

use crate::{
	AdaptedPrices, ConfigRecord, CoreIndex, PotentialRenewalId, RegionId, SaleInfoRecord,
	StatusRecord, Timeslice,
};

/// Trait for providing the reserved core count.
pub trait CoreCountProvider {
	/// Returns the number of reserved cores (reservations + leases).
	fn reserved_core_count() -> CoreIndex;
}

/// Trait for querying renewal rights from the broker.
///
/// The market needs this to determine displacement protection during the Renewal phase.
/// Auction winners with renewal rights cannot be displaced by other renewers.
pub trait RenewalRightsProvider<AccountId> {
	/// Returns the number of renewal rights held by `who` for the given timeslice.
	fn renewal_rights_count(who: &AccountId, when: Timeslice) -> u32;
}

/// Errors specific to market operations.
#[derive(Debug)]
pub enum MarketError {
	NoSales,
	Overpriced,
	BidNotExist,
	Uninitialized,
	TooEarly,
	Unavailable,
	SoldOut,
	/// Operation not allowed in the current sale phase.
	WrongPhase,
	/// Bid price is above the current descending price.
	BidTooHigh,
	/// Bids cannot be lowered or cancelled.
	BidNotCancellable,
}

impl From<MarketError> for DispatchError {
	fn from(value: MarketError) -> Self {
		match value {
			MarketError::NoSales => Self::Other("NoSales"),
			MarketError::Overpriced => Self::Other("Overpriced"),
			MarketError::BidNotExist => Self::Other("BidNotExist"),
			MarketError::Uninitialized => Self::Other("Uninitialized"),
			MarketError::TooEarly => Self::Other("TooEarly"),
			MarketError::Unavailable => Self::Other("Unavailable"),
			MarketError::SoldOut => Self::Other("SoldOut"),
			MarketError::WrongPhase => Self::Other("WrongPhase"),
			MarketError::BidTooHigh => Self::Other("BidTooHigh"),
			MarketError::BidNotCancellable => Self::Other("BidNotCancellable"),
		}
	}
}

/// Result of placing a purchase order.
pub enum OrderResult<Balance, BidId> {
	BidPlaced { id: BidId, bid_price: Balance },
	Sold { price: Balance, region_id: RegionId, region_end: Timeslice },
}

/// Information about a bid that was displaced during the Renewal phase.
pub struct DisplacedBid<AccountId, Balance, BidId> {
	/// The account whose allocation was displaced.
	pub who: AccountId,
	/// The amount to refund (their original bid price).
	pub refund: Balance,
	/// The bid ID that was displaced.
	pub bid_id: BidId,
}

/// Result of placing a renewal order.
pub enum RenewalOrderResult<Balance, BidId, AccountId> {
	BidPlaced {
		id: BidId,
		bid_price: Balance,
	},
	Sold {
		price: Balance,
		next_renewal_price: Balance,
		region_id: RegionId,
		effective_to: Timeslice,
		/// If a renewal displaced an auction winner, contains the displaced bid info.
		/// The broker should refund the displaced bidder.
		displaced: Option<DisplacedBid<AccountId, Balance, BidId>>,
	},
}

/// Result of closing a bid.
pub struct CloseBidResult<AccountId, Balance> {
	pub owner: AccountId,
	pub refund: Balance,
}

/// Actions returned by `Market::tick` for the broker to process.
pub enum TickAction<Balance, BlockNumber, AccountId, BidId> {
	SellRegion {
		owner: AccountId,
		/// How much was paid for this region in total.
		paid: Balance,
		region_id: RegionId,
		region_end: Timeslice,
	},
	RenewRegion {
		owner: AccountId,
		renewal_id: PotentialRenewalId,
	},
	Refund {
		amount: Balance,
		who: AccountId,
	},
	BidClosed {
		id: BidId,
		owner: AccountId,
	},
	SaleRotated {
		old_sale: SaleInfoRecord<Balance, BlockNumber>,
		new_sale: SaleInfoRecord<Balance, BlockNumber>,
		new_prices: AdaptedPrices<Balance>,
		start_price: Balance,
	},
}

/// Data returned when sales are first started.
#[derive(Debug)]
pub struct SalesStarted<Balance, BlockNumber> {
	pub old_sale: SaleInfoRecord<Balance, BlockNumber>,
	pub new_sale: SaleInfoRecord<Balance, BlockNumber>,
	pub new_prices: AdaptedPrices<Balance>,
	pub start_price: Balance,
}

/// Trait representing generic market logic.
///
/// The assumptions for this generic market are:
/// - Every order will either create a bid or will be resolved immediately.
/// - There are two types of orders: bulk coretime purchase and bulk coretime renewal.
/// - Coretime regions are fungible.
/// - The market operates in phases: Market (auction), Renewal, Settlement.
pub trait Market {
	type AccountId;
	type Balance;
	type BlockNumber;
	type Error: Into<DispatchError>;
	/// Unique ID assigned to every bid.
	type BidId: Copy
		+ core::fmt::Debug
		+ Encode
		+ Decode
		+ DecodeWithMemTracking
		+ MaxEncodedLen
		+ TypeInfo
		+ PartialEq
		+ Eq;
	type CoreCount: CoreCountProvider;

	fn start_sales(
		block_number: Self::BlockNumber,
		reserve_price: Self::Balance,
		core_count: CoreIndex,
	) -> Result<SalesStarted<Self::Balance, Self::BlockNumber>, Self::Error>;

	/// Place an order for one bulk coretime region purchase.
	///
	/// During Market phase: creates a bid at the given price. Bids must be <= current
	/// descending price. Returns `BidPlaced`.
	///
	/// - `price_limit` - the bid price (must be <= current descending price)
	fn place_order(
		block_number: Self::BlockNumber,
		who: &Self::AccountId,
		price_limit: Self::Balance,
	) -> Result<OrderResult<Self::Balance, Self::BidId>, Self::Error>;

	/// Place an order for bulk coretime renewal.
	///
	/// During Market phase: creates a bid like `place_order` (renewer participating in auction).
	/// During Renewal phase: exercises renewal right. May displace the lowest non-renewer
	/// auction winner if all cores are allocated.
	fn place_renewal_order(
		block_number: Self::BlockNumber,
		who: &Self::AccountId,
		renewal: PotentialRenewalId,
		recorded_price: Self::Balance,
	) -> Result<RenewalOrderResult<Self::Balance, Self::BidId, Self::AccountId>, Self::Error>;

	/// Raise an existing bid to a higher price.
	///
	/// RFC-17: bids cannot be lowered or cancelled, only raised up to the current
	/// descending price. Returns the additional amount that needs to be locked
	/// (new_price - old_price).
	fn raise_bid(
		block_number: Self::BlockNumber,
		id: Self::BidId,
		who: &Self::AccountId,
		new_price: Self::Balance,
	) -> Result<Self::Balance, Self::Error>;

	/// Close the bid given its `BidId`.
	///
	/// In RFC-17, bids are binding and cannot be cancelled.
	fn close_bid(
		id: Self::BidId,
		maybe_check_owner: Option<Self::AccountId>,
	) -> Result<CloseBidResult<Self::AccountId, Self::Balance>, Self::Error>;

	/// Logic that gets called in `on_initialize` hook.
	fn tick(
		now: Self::BlockNumber,
		weight_meter: &mut WeightMeter,
	) -> Vec<TickAction<Self::Balance, Self::BlockNumber, Self::AccountId, Self::BidId>>;
}

/// Trait for accessing persistent market state needed by broker logic.
pub trait MarketState: Market {
	fn configuration() -> Option<ConfigRecord<Self::BlockNumber>>;
	fn set_configuration(config: ConfigRecord<Self::BlockNumber>);

	fn status() -> Option<StatusRecord>;
	fn set_status(status: StatusRecord);

	fn sale_info() -> Option<SaleInfoRecord<Self::Balance, Self::BlockNumber>>;
	fn set_sale_info(sale_info: SaleInfoRecord<Self::Balance, Self::BlockNumber>);

	fn current_price(block_number: Self::BlockNumber) -> Option<Self::Balance>;

	fn current_phase() -> Option<SalePhase>;
}
