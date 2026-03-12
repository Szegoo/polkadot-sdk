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
		}
	}
}

/// Result of placing a purchase order.
pub enum OrderResult<Balance, BidId> {
	BidPlaced { id: BidId, bid_price: Balance },
	Sold { price: Balance, region_id: RegionId, region_end: Timeslice },
}

/// Result of placing a renewal order.
pub enum RenewalOrderResult<Balance, BidId> {
	BidPlaced {
		id: BidId,
		bid_price: Balance,
	},
	Sold {
		price: Balance,
		next_renewal_price: Balance,
		region_id: RegionId,
		effective_to: Timeslice,
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
	TimesliceCommited {
		timeslice: Timeslice,
	},
	LastTimesliceChanged {
		last_timeslice: Timeslice,
		rc_block: BlockNumber,
	},
}

/// Data returned when sales are first started.
#[derive(Debug)]
pub struct SalesStarted<Balance, BlockNumber> {
	pub imaginary_old_sale: SaleInfoRecord<Balance, BlockNumber>,
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
		end_price: Self::Balance,
		core_count: CoreIndex,
	) -> Result<SalesStarted<Self::Balance, Self::BlockNumber>, Self::Error>;

	/// Place an order for one bulk coretime region purchase.
	///
	/// This method may or may not create a bid, according to the market rules.
	///
	/// - `price_limit` - maximum price which the buyer is willing to pay
	fn place_order(
		block_number: Self::BlockNumber,
		who: &Self::AccountId,
		price_limit: Self::Balance,
	) -> Result<OrderResult<Self::Balance, Self::BidId>, Self::Error>;

	/// Place an order for bulk coretime renewal.
	///
	/// This method may or may not create a bid, according to the market rules.
	fn place_renewal_order(
		block_number: Self::BlockNumber,
		who: &Self::AccountId,
		renewal: PotentialRenewalId,
		recorded_price: Self::Balance,
	) -> Result<RenewalOrderResult<Self::Balance, Self::BidId>, Self::Error>;

	/// Close the bid given its `BidId`.
	///
	/// If the market logic allows creating the bids this method allows to close any bids (either
	/// forcefully if `maybe_check_owner` is `None` or checking the bid owner if it's `Some`).
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
}
