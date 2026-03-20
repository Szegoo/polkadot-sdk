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

//! Legacy market implementation for the broker pallet.
//!
//! This implements the [`Market`] and [`MarketState`] traits directly on the broker pallet,
//! providing the original (pre-RFC-17) coretime sales model. To use it, configure the runtime
//! with `type Market = pallet_broker::Pallet<Runtime>`.

use core::cmp;
use frame_support::{ensure, storage_alias, traits::Get, weights::WeightMeter};
use sp_arithmetic::FixedPointNumber;
use sp_runtime::{traits::Zero, FixedPointOperand, FixedU64, SaturatedConversion, Saturating};

use crate::{
	AdaptPrice, AdaptedPrices, BalanceOf, CenterTargetPrice, CloseBidResult, Config,
	CoreCountProvider, CoreIndex, CoreMask, Leases, Market, MarketError,
	MarketState, OrderResult, Pallet, PotentialRenewalId, RegionId, RelayBlockNumberOf,
	RenewalOrderResult, Reservations, SaleInfoRecord, SalePerformance,
	SalesStarted, StatusRecord, TickAction, Timeslice,
};
use alloc::vec::Vec;

/// Concrete config type for the legacy market.
type LegacyConfigRecordOf<T> = crate::ConfigRecord<RelayBlockNumberOf<T>>;
/// Concrete sale info type for the legacy market.
type LegacySaleInfoRecordOf<T> = SaleInfoRecord<BalanceOf<T>, RelayBlockNumberOf<T>>;

// Storage items for the legacy market. These use `#[storage_alias]` so they don't appear
// in the broker pallet's metadata — they're only used when `type Market = Pallet<Runtime>`.

#[storage_alias]
type Configuration<T: Config> =
	StorageValue<Pallet<T>, crate::ConfigRecord<RelayBlockNumberOf<T>>, frame_support::pallet_prelude::OptionQuery>;

#[storage_alias]
type SaleInfo<T: Config> =
	StorageValue<Pallet<T>, crate::SaleInfoRecord<BalanceOf<T>, RelayBlockNumberOf<T>>, frame_support::pallet_prelude::OptionQuery>;

#[storage_alias]
type Status<T: Config> =
	StorageValue<Pallet<T>, StatusRecord, frame_support::pallet_prelude::OptionQuery>;

/// Type alias for TickAction with concrete pallet types.
pub(crate) type TickActionOf<T> = TickAction<
	BalanceOf<T>,
	<T as frame_system::Config>::AccountId,
	(),
	SaleInfoRecord<BalanceOf<T>, RelayBlockNumberOf<T>>,
>;

/// Provides the reserved core count from the broker's own storage.
pub struct BrokerCoreCountProvider<T>(core::marker::PhantomData<T>);

impl<T: Config> CoreCountProvider for BrokerCoreCountProvider<T> {
	fn reserved_core_count() -> CoreIndex {
		Reservations::<T>::decode_len().unwrap_or_default() as CoreIndex +
			Leases::<T>::decode_len().unwrap_or_default() as CoreIndex
	}
}

impl<T: Config> Market for Pallet<T>
where
	BalanceOf<T>: FixedPointOperand,
{
	type AccountId = T::AccountId;
	type Balance = BalanceOf<T>;
	type BlockNumber = RelayBlockNumberOf<T>;
	type Error = MarketError;
	/// Must be unique.
	type BidId = ();
	type CoreCount = BrokerCoreCountProvider<T>;
	type Config = crate::ConfigRecord<RelayBlockNumberOf<T>>;
	type SaleInfo = crate::SaleInfoRecord<BalanceOf<T>, RelayBlockNumberOf<T>>;

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

		// Imaginary old sale for bootstrapping the first actual sale:
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

		let start_price = sell_price::<T>(block_number, &new_sale, &config);

		Ok(SalesStarted { old_sale, new_sale, new_prices, start_price })
	}

	fn place_order(
		block_number: RelayBlockNumberOf<T>,
		_who: &T::AccountId,
		price_limit: BalanceOf<T>,
	) -> Result<OrderResult<BalanceOf<T>, Self::BidId>, Self::Error> {
		let config = Configuration::<T>::get().ok_or(MarketError::Uninitialized)?;
		let mut sale = SaleInfo::<T>::get().ok_or(MarketError::NoSales)?;
		let status = Status::<T>::get().ok_or(MarketError::Uninitialized)?;

		ensure!(sale.first_core < status.core_count, MarketError::Unavailable);
		ensure!(sale.cores_sold < sale.cores_offered, MarketError::SoldOut);

		ensure!(block_number > sale.sale_start, MarketError::TooEarly);

		let current_price = sell_price::<T>(block_number, &sale, &config);
		let bid_price = price_limit.min(current_price);

		let core = purchase_core::<T>(bid_price, &mut sale);
		SaleInfo::<T>::put(&sale);

		let region_id = RegionId { begin: sale.region_begin, core, mask: CoreMask::complete() };

		Ok(OrderResult::Sold { price: bid_price, region_id, region_end: sale.region_end })
	}

	fn place_renewal_order(
		block_number: RelayBlockNumberOf<T>,
		_who: &T::AccountId,
		_renewal: PotentialRenewalId,
		recorded_price: BalanceOf<T>,
	) -> Result<RenewalOrderResult<BalanceOf<T>, Self::BidId, Self::AccountId>, Self::Error> {
		let config = Configuration::<T>::get().ok_or(MarketError::Uninitialized)?;
		let status = Status::<T>::get().ok_or(MarketError::Uninitialized)?;
		let mut sale = SaleInfo::<T>::get().ok_or(MarketError::NoSales)?;

		ensure!(sale.first_core < status.core_count, MarketError::Unavailable);
		ensure!(sale.cores_sold < sale.cores_offered, MarketError::SoldOut);

		let price_cap =
			cmp::max(recorded_price + config.penalty * recorded_price, sale.reserve_price);
		let current_price = sell_price::<T>(block_number, &sale, &config);
		let next_renewal_price = current_price.min(price_cap);

		let core = purchase_core::<T>(recorded_price, &mut sale);
		SaleInfo::<T>::put(&sale);

		let region_id = RegionId { core, begin: sale.region_begin, mask: CoreMask::complete() };

		Ok(RenewalOrderResult::Sold {
			price: recorded_price,
			next_renewal_price,
			region_id,
			effective_to: sale.region_end,
			displaced: None,
		})
	}

	fn raise_bid(
		_block_number: RelayBlockNumberOf<T>,
		_id: Self::BidId,
		_who: &Self::AccountId,
		_new_price: BalanceOf<T>,
	) -> Result<BalanceOf<T>, Self::Error> {
		Err(MarketError::BidNotExist)
	}

	fn close_bid(
		_id: Self::BidId,
		_maybe_check_owner: Option<T::AccountId>,
	) -> Result<CloseBidResult<T::AccountId, BalanceOf<T>>, Self::Error> {
		Err(MarketError::BidNotExist)
	}

	fn tick(
		block_number: RelayBlockNumberOf<T>,
		_weight_meter: &mut WeightMeter,
	) -> Vec<TickActionOf<T>> {
		let (Some(config), Some(mut status)) = (Configuration::<T>::get(), Status::<T>::get())
		else {
			return alloc::vec![];
		};

		let mut actions = alloc::vec![];

		if let Some(commit_timeslice) =
			next_timeslice_to_commit::<T>(block_number, &config, &status)
		{
			status.last_committed_timeslice = commit_timeslice;

			if let Some(sale) = SaleInfo::<T>::get() {
				if commit_timeslice >= sale.region_begin {
					// Process renewals against the current sale before rotating.
					actions.push(TickAction::ProcessRenewals);

					sale_rotated::<T>(sale, &config, &status, block_number, &mut actions);
				}
			}
		}

		let current_timeslice = current_timeslice::<T>(block_number);
		if status.last_timeslice < current_timeslice {
			status.last_timeslice.saturating_inc();
		}

		Status::<T>::put(status);

		actions
	}
}

impl<T: Config> MarketState for Pallet<T>
where
	BalanceOf<T>: FixedPointOperand,
{
	fn configuration() -> Option<LegacyConfigRecordOf<T>> {
		Configuration::<T>::get()
	}

	fn set_configuration(config: LegacyConfigRecordOf<T>) {
		Configuration::<T>::put(config);
	}

	fn status() -> Option<StatusRecord> {
		Status::<T>::get()
	}

	fn set_status(status: StatusRecord) {
		Status::<T>::put(status);
	}

	fn sale_info() -> Option<LegacySaleInfoRecordOf<T>> {
		SaleInfo::<T>::get()
	}

	fn set_sale_info(sale_info: LegacySaleInfoRecordOf<T>) {
		SaleInfo::<T>::put(sale_info);
	}

	fn current_price(block_number: RelayBlockNumberOf<T>) -> Option<BalanceOf<T>> {
		let config = Configuration::<T>::get()?;
		let sale = SaleInfo::<T>::get()?;
		Some(sell_price::<T>(block_number, &sale, &config))
	}

	#[cfg(feature = "runtime-benchmarks")]
	fn benchmark_config() -> Self::Config {
		crate::ConfigRecord {
			advance_notice: 2u32.into(),
			market_period: 1u32.into(),
			renewal_period: 1u32.into(),
			ideal_bulk_proportion: Default::default(),
			limit_cores_offered: None,
			region_length: 3,
			penalty: sp_arithmetic::Perbill::from_percent(10),
			contribution_timeout: 5,
		}
	}
}

pub(crate) fn sale_rotated<T: Config>(
	sale: LegacySaleInfoRecordOf<T>,
	config: &LegacyConfigRecordOf<T>,
	status: &StatusRecord,
	block_number: RelayBlockNumberOf<T>,
	actions: &mut Vec<TickActionOf<T>>,
) where
	BalanceOf<T>: FixedPointOperand,
{
	let reserved_cores = <Pallet<T> as Market>::CoreCount::reserved_core_count();
	let (new_prices, new_sale) =
		rotate_sale::<T>(&sale, config, status, reserved_cores, block_number);
	SaleInfo::<T>::put(&new_sale);

	let start_price = sell_price::<T>(block_number, &new_sale, config);
	actions.push(TickAction::SaleRotated { old_sale: sale, new_sale, new_prices, start_price });
}

fn purchase_core<T: Config>(price: BalanceOf<T>, sale: &mut LegacySaleInfoRecordOf<T>) -> CoreIndex {
	let core = sale.first_core.saturating_add(sale.cores_sold);
	sale.cores_sold.saturating_inc();
	if sale.cores_sold <= sale.ideal_cores_sold || sale.clearing_price.is_none() {
		sale.clearing_price = Some(price);
	}
	core
}

pub(crate) fn sell_price<T: Config>(
	now: RelayBlockNumberOf<T>,
	sale: &LegacySaleInfoRecordOf<T>,
	config: &LegacyConfigRecordOf<T>,
) -> BalanceOf<T>
where
	BalanceOf<T>: FixedPointOperand,
{
	let num = now.saturating_sub(sale.sale_start).min(config.market_period).saturated_into();
	let through = FixedU64::from_rational(num, config.market_period.saturated_into());
	leadin_factor_at(through).saturating_mul_int(sale.reserve_price)
}

pub(crate) fn leadin_factor_at(when: FixedU64) -> FixedU64 {
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
	config: &LegacyConfigRecordOf<T>,
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
	config: &LegacyConfigRecordOf<T>,
) -> Timeslice {
	let advanced = now.saturating_add(config.advance_notice);
	let timeslice_period = T::TimeslicePeriod::get();
	(advanced / timeslice_period).saturated_into()
}

fn adapt_prices<T: Config>(old_sale: &LegacySaleInfoRecordOf<T>) -> AdaptedPrices<BalanceOf<T>>
where
	BalanceOf<T>: FixedPointOperand,
{
	CenterTargetPrice::<BalanceOf<T>>::adapt_price(SalePerformance::from_sale(old_sale))
}

pub(crate) fn rotate_sale<T: Config>(
	old_sale: &LegacySaleInfoRecordOf<T>,
	config: &LegacyConfigRecordOf<T>,
	status: &StatusRecord,
	reserved_cores: CoreIndex,
	now: RelayBlockNumberOf<T>,
) -> (AdaptedPrices<BalanceOf<T>>, LegacySaleInfoRecordOf<T>)
where
	BalanceOf<T>: FixedPointOperand,
{
	let new_prices = adapt_prices::<T>(old_sale);

	let max_possible_sales = status.core_count.saturating_sub(reserved_cores);
	let limit_cores_offered = config.limit_cores_offered.unwrap_or(CoreIndex::max_value());
	let cores_offered = limit_cores_offered.min(max_possible_sales);
	let sale_start = now;
	let ideal_cores_sold = (config.ideal_bulk_proportion * cores_offered as u32) as u16;

	let opening_price =
		leadin_factor_at(FixedU64::zero()).saturating_mul_int(new_prices.reserve_price);
	let clearing_price = if cores_offered > 0 {
		// No core sold -> price was too high -> we have to adjust downwards.
		Some(new_prices.reserve_price)
	} else {
		None
	};

	let region_begin = old_sale.region_end;
	let region_end = region_begin + config.region_length;

	let new_sale = SaleInfoRecord {
		sale_start,
		opening_price,
		reserve_price: new_prices.reserve_price,
		clearing_price,
		region_begin,
		region_end,
		first_core: reserved_cores,
		ideal_cores_sold,
		cores_offered,
		cores_sold: 0,
	};

	(new_prices, new_sale)
}
