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

use core::cmp;
use frame_support::ensure;
use frame_system::pallet_prelude::AccountIdFor;
use sp_arithmetic::FixedPointNumber;
use sp_core::Get;
use sp_runtime::{traits::Zero, DispatchError, FixedU64, SaturatedConversion, Saturating};

use crate::{
	AdaptPrice, AdaptedPrices, BalanceOf, Config, ConfigRecordOf, Configuration, CoreIndex, Leases,
	Pallet, PotentialRenewalId, RelayBlockNumberOf, Reservations, SaleInfo, SaleInfoRecord,
	SaleInfoRecordOf, SalePerformance, Status, StatusRecord, Timeslice,
};

// TODO: Extend the documentation.

/// Trait representig generic market logic.
///
/// The assumptions for this generic market are:
/// - Every order will either create a bid or will be resolved immediately.
/// - There're two types of orders: bulk coretime purchase and bulk coretime renewal.
/// - Coretime regions are fungible.
pub trait Market<Balance, RelayBlockNumber, AccountId> {
	type Error: Into<DispatchError>;
	/// Unique ID assigned to every bid.
	type BidId;

	// TODO: Unify the interface.
	fn start_sales(
		block_number: RelayBlockNumber,
		end_price: Balance,
		core_count: u16,
	) -> Result<(), Self::Error>;

	/// Place an order for one bulk coretime region purchase.
	///
	/// This method may or may not create a bid, according to the market rules.
	///
	/// - `since_timeslice_start` - amount of blocks passed since the current timeslice start
	/// - `price_limit` - maximum price which the buyer is willing to pay
	fn place_order(
		block_number: RelayBlockNumber,
		who: &AccountId,
		price_limit: Balance,
	) -> Result<OrderResult<Balance, Self::BidId>, Self::Error>;

	/// Place an order for bulk coretime renewal.
	///
	/// This method may or may not create a bid, according to the market rules.
	///
	/// - `since_timeslice_start` - amount of blocks passed since the current timeslice start
	fn place_renewal_order(
		block_number: RelayBlockNumber,
		who: &AccountId,
		renewal: PotentialRenewalId,
		recorded_price: Balance,
	) -> Result<RenewalOrderResult<Balance, Self::BidId>, Self::Error>;

	/// Close the bid given its `BidId`.
	///
	/// If the market logic allows creating the bids this method allows to close any bids (either
	/// forcefully if `maybe_check_owner` is `None` or checking the bid owner if it's `Some`).
	fn close_bid(
		id: Self::BidId,
		maybe_check_owner: Option<AccountId>,
	) -> Result<CloseBidResult<AccountId, Balance>, Self::Error>;

	/// Logic that gets called in `on_initialize` hook.
	fn tick(now: RelayBlockNumber) -> Vec<TickAction<AccountId, Balance, Self::BidId>>;
}

pub enum OrderResult<Balance, BidId> {
	BidPlaced { id: BidId, bid_price: Balance },
	Sold { price: Balance, region_begin: Timeslice, region_end: Timeslice, core: CoreIndex },
}

pub enum RenewalOrderResult<Balance, BidId> {
	BidPlaced {
		id: BidId,
		bid_price: Balance,
	},
	Sold {
		price: Balance,
		next_renewal_price: Balance,
		/// Timeslice where the newly renewed coretime will be active.
		effective_from: Timeslice,
		effective_to: Timeslice,
		core: CoreIndex,
	},
}

pub struct CloseBidResult<AccountId, Balance> {
	pub owner: AccountId,
	pub refund: Balance,
}
pub enum TickAction<AccountId, Balance, BidId> {
	SellRegion { owner: AccountId, refund: Balance },
	RenewRegion { owner: AccountId, renewal_id: PotentialRenewalId, refund: Balance },
	BidClosed { id: BidId, refund: Balance, owner: AccountId },
}

pub enum MarketError {
	NoSales,
	Overpriced,
	BidNotExist,
	Uninitialized,
	TooEarly,
	Unavailable,
	SoldOut,
}

// TODO: Proper conversion
impl From<MarketError> for DispatchError {
	fn from(value: MarketError) -> Self {
		DispatchError::Other("TODO")
	}
}

impl<T: Config> Market<BalanceOf<T>, RelayBlockNumberOf<T>, AccountIdFor<T>> for Pallet<T> {
	type Error = MarketError;
	/// Must be unique.
	type BidId = ();

	fn start_sales(
		block_number: RelayBlockNumberOf<T>,
		end_price: BalanceOf<T>,
		core_count: u16,
	) -> Result<(), Self::Error> {
		let config = Configuration::<T>::get().ok_or(MarketError::Uninitialized)?;

		let commit_timeslice = latest_timeslice_ready_to_commit::<T>(block_number, &config);
		let status = StatusRecord {
			core_count,
			private_pool_size: 0,
			system_pool_size: 0,
			last_committed_timeslice: commit_timeslice.saturating_sub(1),
			last_timeslice: current_timeslice::<T>(block_number),
		};
		// Imaginary old sale for bootstrapping the first actual sale:
		let old_sale = SaleInfoRecord {
			sale_start: block_number,
			leadin_length: Zero::zero(),
			end_price,
			sellout_price: None,
			region_begin: commit_timeslice,
			region_end: commit_timeslice.saturating_add(config.region_length),
			first_core: 0,
			ideal_cores_sold: 0,
			cores_offered: 0,
			cores_sold: 0,
		};

		let (new_prices, new_sale) = rotate_sale::<T>(&old_sale, &config, &status, block_number);
		// TODO: call OnRotateSale (Market associated type).
		Self::rotate_sale(old_sale, &new_sale, new_prices, &config, &status);

		Status::<T>::put(&status);

		Ok(())
	}

	fn place_order(
		block_number: RelayBlockNumberOf<T>,
		who: &AccountIdFor<T>,
		price_limit: BalanceOf<T>,
	) -> Result<OrderResult<BalanceOf<T>, Self::BidId>, Self::Error> {
		let mut sale = SaleInfo::<T>::get().ok_or(MarketError::NoSales)?;
		// TODO: don't read status here.
		let status = Status::<T>::get().ok_or(MarketError::Uninitialized)?;

		ensure!(sale.first_core < status.core_count, MarketError::Unavailable);
		ensure!(sale.cores_sold < sale.cores_offered, MarketError::SoldOut);

		ensure!(block_number > sale.sale_start, MarketError::TooEarly);

		let sell_price = sell_price::<T>(block_number, &sale)?;

		if price_limit < sell_price {
			return Err(MarketError::Overpriced)
		};

		let core = purchase_core::<T>(who, sell_price, &mut sale);
		SaleInfo::<T>::put(&sale);

		Ok(OrderResult::Sold {
			price: sell_price,
			region_begin: sale.region_begin,
			region_end: sale.region_end,
			core,
		})
	}

	// TODO: If we return Sold also return optional argument showing whether we should create a new
	// potential renewal or not.
	fn place_renewal_order(
		block_number: RelayBlockNumberOf<T>,
		who: &AccountIdFor<T>,
		renewal: PotentialRenewalId,
		recorded_price: BalanceOf<T>,
	) -> Result<RenewalOrderResult<BalanceOf<T>, Self::BidId>, Self::Error> {
		// TODO: Store `config.renewal_bump` in the market config.
		let config = Configuration::<T>::get().ok_or(MarketError::Uninitialized)?;
		// TODO: don't read status here.
		let status = Status::<T>::get().ok_or(MarketError::Uninitialized)?;
		// TODO: Don't access main pallet storage here.
		let mut sale = SaleInfo::<T>::get().ok_or(MarketError::NoSales)?;

		ensure!(sale.first_core < status.core_count, MarketError::Unavailable);
		ensure!(sale.cores_sold < sale.cores_offered, MarketError::SoldOut);

		let price_cap =
			cmp::max(recorded_price + config.renewal_bump * recorded_price, sale.end_price);
		let sell_price = sell_price::<T>(block_number, &sale)?;
		let next_renewal_price = sell_price.min(price_cap);

		let core = purchase_core::<T>(who, recorded_price, &mut sale);
		SaleInfo::<T>::put(&sale);

		return Ok(RenewalOrderResult::Sold {
			price: recorded_price,
			next_renewal_price,
			effective_from: sale.region_begin,
			effective_to: sale.region_end,
			core,
		})
	}

	fn close_bid(
		id: Self::BidId,
		maybe_check_owner: Option<AccountIdFor<T>>,
	) -> Result<CloseBidResult<AccountIdFor<T>, BalanceOf<T>>, Self::Error> {
		Err(MarketError::BidNotExist)
	}

	fn tick(
		now: RelayBlockNumberOf<T>,
	) -> Vec<TickAction<AccountIdFor<T>, BalanceOf<T>, Self::BidId>> {
		// TODO: Store `config.renewal_bump` in the market config.
		let config = Configuration::<T>::get().unwrap();
		// TODO: don't read status here.
		let mut status = Status::<T>::get().unwrap();

		if let Some(commit_timeslice) = Self::next_timeslice_to_commit(&config, &status) {
			status.last_committed_timeslice = commit_timeslice;
			if let Some(sale) = SaleInfo::<T>::get() {
				if commit_timeslice >= sale.region_begin {
					let (new_prices, new_sale) = rotate_sale::<T>(&sale, &config, &status, now);
					// TODO: call OnRotateSale (Market associated type).
					Self::rotate_sale(sale, &new_sale, new_prices, &config, &status);
				}
			}

			// TODO: Call some hook instead (and move the code somewhere else).
			Self::process_pool(commit_timeslice, &mut status);

			let timeslice_period = T::TimeslicePeriod::get();
			let rc_begin = RelayBlockNumberOf::<T>::from(commit_timeslice) * timeslice_period;
			for core in 0..status.core_count {
				Self::process_core_schedule(commit_timeslice, rc_begin, core);
			}
		}

		Status::<T>::put(status);

		vec![]
	}
}

fn purchase_core<T: Config>(
	who: &T::AccountId,
	price: BalanceOf<T>,
	sale: &mut SaleInfoRecordOf<T>,
) -> CoreIndex {
	let core = sale.first_core.saturating_add(sale.cores_sold);
	sale.cores_sold.saturating_inc();
	if sale.cores_sold <= sale.ideal_cores_sold || sale.sellout_price.is_none() {
		sale.sellout_price = Some(price);
	}
	core
}

fn sell_price<T: Config>(
	now: RelayBlockNumberOf<T>,
	sale: &SaleInfoRecordOf<T>,
) -> Result<BalanceOf<T>, MarketError> {
	// TODO: Store this info in the dedicated storage item?
	let sale = SaleInfo::<T>::get().ok_or(MarketError::NoSales)?;

	let num = now.saturating_sub(sale.sale_start).min(sale.leadin_length).saturated_into();
	let through = FixedU64::from_rational(num, sale.leadin_length.saturated_into());
	Ok(leadin_factor_at(through).saturating_mul_int(sale.end_price))
}

fn leadin_factor_at(when: FixedU64) -> FixedU64 {
	if when <= FixedU64::from_rational(1, 2) {
		FixedU64::from(100).saturating_sub(when.saturating_mul(180.into()))
	} else {
		FixedU64::from(19).saturating_sub(when.saturating_mul(18.into()))
	}
}

fn current_timeslice<T: Config>(now: RelayBlockNumberOf<T>) -> Timeslice {
	let timeslice_period = T::TimeslicePeriod::get();
	(now / timeslice_period).saturated_into()
}

fn next_timeslice_to_commit<T: Config>(
	now: RelayBlockNumberOf<T>,
	config: &ConfigRecordOf<T>,
	status: &StatusRecord,
) -> Option<Timeslice> {
	if status.last_committed_timeslice < latest_timeslice_ready_to_commit::<T>(now, config) {
		Some(status.last_committed_timeslice + 1)
	} else {
		None
	}
}

fn latest_timeslice_ready_to_commit<T: Config>(
	now: RelayBlockNumberOf<T>,
	config: &ConfigRecordOf<T>,
) -> Timeslice {
	let advanced = now.saturating_add(config.advance_notice);
	let timeslice_period = T::TimeslicePeriod::get();
	(advanced / timeslice_period).saturated_into()
}

// TODO: Don't rely on the pallet config?
fn adapt_prices<T: Config>(old_sale: &SaleInfoRecordOf<T>) -> AdaptedPrices<BalanceOf<T>> {
	// Calculate the start price for the upcoming sale.
	let new_prices = T::PriceAdapter::adapt_price(SalePerformance::from_sale(&old_sale));

	log::debug!(
		"Rotated sale, new prices: {:?}, {:?}",
		new_prices.end_price,
		new_prices.target_price
	);

	new_prices
}

// TODO: MUSTN'T BE PUB - it's a workaround to call it from the do_start_sales.
pub fn rotate_sale<T: Config>(
	old_sale: &SaleInfoRecordOf<T>,
	config: &ConfigRecordOf<T>,
	status: &StatusRecord,
	now: RelayBlockNumberOf<T>,
) -> (AdaptedPrices<BalanceOf<T>>, SaleInfoRecordOf<T>) {
	let new_prices = adapt_prices::<T>(&old_sale);

	// TODO: Move this logic somewhere else.
	let first_core = Reservations::<T>::decode_len().unwrap_or_default() as u16 +
		Leases::<T>::decode_len().unwrap_or_default() as u16;

	let max_possible_sales = status.core_count.saturating_sub(first_core);
	let limit_cores_offered = config.limit_cores_offered.unwrap_or(CoreIndex::max_value());
	let cores_offered = limit_cores_offered.min(max_possible_sales);
	let sale_start = now.saturating_add(config.interlude_length);
	let leadin_length = config.leadin_length;
	let ideal_cores_sold = (config.ideal_bulk_proportion * cores_offered as u32) as u16;
	let sellout_price = if cores_offered > 0 {
		// No core sold -> price was too high -> we have to adjust downwards.
		Some(new_prices.end_price)
	} else {
		None
	};

	let region_begin = old_sale.region_end;
	let region_end = region_begin + config.region_length;

	let new_sale = SaleInfoRecord {
		sale_start,
		leadin_length,
		end_price: new_prices.end_price,
		sellout_price,
		region_begin,
		region_end,
		first_core,
		ideal_cores_sold,
		cores_offered,
		cores_sold: 0,
	};

	SaleInfo::<T>::put(&new_sale);

	(new_prices, new_sale)
}
