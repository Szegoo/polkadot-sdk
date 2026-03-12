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

#![cfg_attr(not(feature = "std"), no_std)]

pub use pallet::*;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

extern crate alloc;

use alloc::{vec, vec::Vec};
use core::cmp;
use frame_support::{
	ensure,
	traits::{tokens::Balance as BalanceT, Get},
	weights::{Weight, WeightMeter},
};
use sp_arithmetic::FixedPointNumber;
use sp_coretime::{
	AdaptPrice, AdaptedPrices, CloseBidResult, ConfigRecord, CoreCountProvider, CoreIndex,
	CoreMask, Market, MarketError, MarketState, OrderResult, PotentialRenewalId, RegionId,
	RenewalOrderResult, SaleInfoRecord, SalePerformance, SalesStarted, StatusRecord, TickAction,
	Timeslice,
};
use sp_runtime::{
	traits::{AtLeast32BitUnsigned, SaturatedConversion, Saturating, Zero},
	FixedPointOperand, FixedU64,
};

type BalanceOf<T> = <T as pallet::Config>::Balance;
type RelayBlockNumberOf<T> = <T as pallet::Config>::RelayBlockNumber;
type ConfigRecordOf<T> = ConfigRecord<RelayBlockNumberOf<T>>;
type SaleInfoRecordOf<T> = SaleInfoRecord<BalanceOf<T>, RelayBlockNumberOf<T>>;
type TickActionOf<T> =
	TickAction<BalanceOf<T>, RelayBlockNumberOf<T>, <T as frame_system::Config>::AccountId, ()>;

pub trait WeightInfo {
	fn market_sale_rotated() -> Weight;
	fn market_last_timeslice_changed() -> Weight;
}

impl WeightInfo for () {
	fn market_sale_rotated() -> Weight {
		Weight::zero()
	}

	fn market_last_timeslice_changed() -> Weight {
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
		type Balance: BalanceT + FixedPointOperand;
		type RelayBlockNumber: Parameter
			+ MaxEncodedLen
			+ AtLeast32BitUnsigned
			+ FixedPointOperand
			+ Copy;
		type WeightInfo: WeightInfo;
		type PriceAdapter: AdaptPrice<Self::Balance>;
		type CoreCountProvider: CoreCountProvider;

		#[pallet::constant]
		type TimeslicePeriod: Get<Self::RelayBlockNumber>;
	}

	/// The current configuration of the coretime market.
	#[pallet::storage]
	pub type Configuration<T> = StorageValue<_, ConfigRecordOf<T>, OptionQuery>;

	/// The current status of the market.
	#[pallet::storage]
	pub type Status<T> = StorageValue<_, StatusRecord, OptionQuery>;

	/// The details of the current sale, including its properties and status.
	#[pallet::storage]
	pub type SaleInfo<T> = StorageValue<_, SaleInfoRecordOf<T>, OptionQuery>;
}

impl<T: Config> Market for Pallet<T> {
	type AccountId = T::AccountId;
	type Balance = BalanceOf<T>;
	type BlockNumber = RelayBlockNumberOf<T>;
	type Error = MarketError;
	type BidId = ();
	type CoreCount = T::CoreCountProvider;

	fn start_sales(
		block_number: RelayBlockNumberOf<T>,
		end_price: BalanceOf<T>,
		core_count: CoreIndex,
	) -> Result<SalesStarted<BalanceOf<T>, RelayBlockNumberOf<T>>, Self::Error> {
		let config = Configuration::<T>::get().ok_or(MarketError::Uninitialized)?;

		let commit_timeslice = latest_timeslice_ready_to_commit::<T>(block_number, &config);
		let status = StatusRecord {
			core_count,
			private_pool_size: 0,
			system_pool_size: 0,
			last_committed_timeslice: commit_timeslice.saturating_sub(1),
			last_timeslice: current_timeslice::<T>(block_number),
		};
		// Imaginary old sale for bootstrapping the first actual sale.
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

		let reserved_cores = Self::CoreCount::reserved_core_count();
		let (new_prices, new_sale) =
			rotate_sale::<T>(&old_sale, &config, &status, reserved_cores, block_number);
		SaleInfo::<T>::put(&new_sale);
		Status::<T>::put(&status);

		let start_price = sell_price::<T>(block_number, &new_sale);

		Ok(SalesStarted { imaginary_old_sale: old_sale, new_sale, new_prices, start_price })
	}

	fn place_order(
		block_number: RelayBlockNumberOf<T>,
		_who: &T::AccountId,
		price_limit: BalanceOf<T>,
	) -> Result<OrderResult<BalanceOf<T>, Self::BidId>, Self::Error> {
		let mut sale = SaleInfo::<T>::get().ok_or(MarketError::NoSales)?;
		let status = Status::<T>::get().ok_or(MarketError::Uninitialized)?;

		ensure!(sale.first_core < status.core_count, MarketError::Unavailable);
		ensure!(sale.cores_sold < sale.cores_offered, MarketError::SoldOut);
		ensure!(block_number > sale.sale_start, MarketError::TooEarly);

		let current_price = sell_price::<T>(block_number, &sale);
		if price_limit < current_price {
			return Err(MarketError::Overpriced);
		}

		let core = purchase_core::<T>(current_price, &mut sale);
		SaleInfo::<T>::put(&sale);

		let region_id = RegionId { begin: sale.region_begin, core, mask: CoreMask::complete() };

		Ok(OrderResult::Sold { price: current_price, region_id, region_end: sale.region_end })
	}

	fn place_renewal_order(
		block_number: RelayBlockNumberOf<T>,
		_who: &T::AccountId,
		_renewal: PotentialRenewalId,
		recorded_price: BalanceOf<T>,
	) -> Result<RenewalOrderResult<BalanceOf<T>, Self::BidId>, Self::Error> {
		let config = Configuration::<T>::get().ok_or(MarketError::Uninitialized)?;
		let status = Status::<T>::get().ok_or(MarketError::Uninitialized)?;
		let mut sale = SaleInfo::<T>::get().ok_or(MarketError::NoSales)?;

		ensure!(sale.first_core < status.core_count, MarketError::Unavailable);
		ensure!(sale.cores_sold < sale.cores_offered, MarketError::SoldOut);

		let price_cap =
			cmp::max(recorded_price + config.renewal_bump * recorded_price, sale.end_price);
		let current_price = sell_price::<T>(block_number, &sale);
		let next_renewal_price = current_price.min(price_cap);

		let core = purchase_core::<T>(recorded_price, &mut sale);
		SaleInfo::<T>::put(&sale);

		let region_id = RegionId { core, begin: sale.region_begin, mask: CoreMask::complete() };

		Ok(RenewalOrderResult::Sold {
			price: recorded_price,
			next_renewal_price,
			region_id,
			effective_to: sale.region_end,
		})
	}

	fn close_bid(
		_id: Self::BidId,
		_maybe_check_owner: Option<T::AccountId>,
	) -> Result<CloseBidResult<T::AccountId, BalanceOf<T>>, Self::Error> {
		Err(MarketError::BidNotExist)
	}

	fn tick(
		block_number: RelayBlockNumberOf<T>,
		weight_meter: &mut WeightMeter,
	) -> Vec<TickActionOf<T>> {
		let (Some(config), Some(mut status)) = (Configuration::<T>::get(), Status::<T>::get())
		else {
			return vec![];
		};

		let mut actions = vec![];

		if let Some(commit_timeslice) =
			next_timeslice_to_commit::<T>(block_number, &config, &status)
		{
			status.last_committed_timeslice = commit_timeslice;

			if let Some(sale) = SaleInfo::<T>::get() {
				if commit_timeslice >= sale.region_begin {
					weight_meter.consume(T::WeightInfo::market_sale_rotated());

					let reserved_cores = Self::CoreCount::reserved_core_count();
					let (new_prices, new_sale) =
						rotate_sale::<T>(&sale, &config, &status, reserved_cores, block_number);
					SaleInfo::<T>::put(&new_sale);

					let start_price = sell_price::<T>(block_number, &new_sale);
					actions.push(TickAction::SaleRotated {
						old_sale: sale,
						new_sale,
						new_prices,
						start_price,
					});
				}
			}

			actions.push(TickAction::TimesliceCommited { timeslice: commit_timeslice });
		}

		let current_timeslice = current_timeslice::<T>(block_number);
		if status.last_timeslice < current_timeslice {
			weight_meter.consume(T::WeightInfo::market_last_timeslice_changed());
			status.last_timeslice.saturating_inc();
			let rc_block = T::TimeslicePeriod::get() * status.last_timeslice.into();
			actions.push(TickAction::LastTimesliceChanged {
				last_timeslice: status.last_timeslice,
				rc_block,
			});
		}

		Status::<T>::put(status);

		actions
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
		SaleInfo::<T>::get().map(|sale| sell_price::<T>(block_number, &sale))
	}
}

fn purchase_core<T: Config>(price: BalanceOf<T>, sale: &mut SaleInfoRecordOf<T>) -> CoreIndex {
	let core = sale.first_core.saturating_add(sale.cores_sold);
	sale.cores_sold.saturating_inc();
	if sale.cores_sold <= sale.ideal_cores_sold || sale.sellout_price.is_none() {
		sale.sellout_price = Some(price);
	}
	core
}

fn sell_price<T: Config>(now: RelayBlockNumberOf<T>, sale: &SaleInfoRecordOf<T>) -> BalanceOf<T> {
	let num = now.saturating_sub(sale.sale_start).min(sale.leadin_length).saturated_into();
	let through = FixedU64::from_rational(num, sale.leadin_length.saturated_into());
	leadin_factor_at(through).saturating_mul_int(sale.end_price)
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

fn adapt_prices<T: Config>(old_sale: &SaleInfoRecordOf<T>) -> AdaptedPrices<BalanceOf<T>> {
	T::PriceAdapter::adapt_price(SalePerformance::from_sale(old_sale))
}

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
	let sale_start = now.saturating_add(config.interlude_length);
	let leadin_length = config.leadin_length;
	let ideal_cores_sold = (config.ideal_bulk_proportion * cores_offered as u32) as u16;
	let sellout_price = if cores_offered > 0 { Some(new_prices.end_price) } else { None };

	let region_begin = old_sale.region_end;
	let region_end = region_begin + config.region_length;

	let new_sale = SaleInfoRecord {
		sale_start,
		leadin_length,
		end_price: new_prices.end_price,
		sellout_price,
		region_begin,
		region_end,
		first_core: reserved_cores,
		ideal_cores_sold,
		cores_offered,
		cores_sold: 0,
	};

	(new_prices, new_sale)
}
