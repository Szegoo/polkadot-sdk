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

#![cfg(test)]

use crate::mock::*;
use frame_support::assert_ok;
use sp_arithmetic::Perbill;
use sp_coretime::{
	ConfigRecord, CoreMask, Market, MarketError, MarketState, OrderResult, PotentialRenewalId,
	RenewalOrderResult, SaleInfoRecord, StatusRecord,
};
use sp_runtime::DispatchError;

type CoretimeMarketImpl = CoretimeMarket;

fn start_sales(end_price: u64, extra_cores: u16) {
	let core_count = extra_cores;
	assert_ok!(CoretimeMarketImpl::start_sales(0, end_price, core_count).map_err(
		|e| -> DispatchError { e.into() }
	));
}

fn place_order(block_number: u64, who: u64, price_limit: u64) -> Result<u64, MarketError> {
	match CoretimeMarketImpl::place_order(block_number, &who, price_limit)? {
		OrderResult::Sold { price, .. } => Ok(price),
		OrderResult::BidPlaced { .. } => unreachable!("This market never places bids"),
	}
}

fn place_order_full(
	block_number: u64,
	who: u64,
	price_limit: u64,
) -> Result<OrderResult<u64, ()>, MarketError> {
	CoretimeMarketImpl::place_order(block_number, &who, price_limit)
}

fn place_renewal_order(
	block_number: u64,
	who: u64,
	recorded_price: u64,
) -> Result<RenewalOrderResult<u64, ()>, MarketError> {
	let renewal_id = PotentialRenewalId { core: 0, when: 0 };
	CoretimeMarketImpl::place_renewal_order(block_number, &who, renewal_id, recorded_price)
}

#[test]
fn place_order_requires_valid_status_and_sale_info() {
	TestExt::new().execute_with(|| {
		// No sale info set yet.
		assert!(matches!(place_order(1, 1, 100), Err(MarketError::NoSales)));

		let status = StatusRecord {
			core_count: 2,
			private_pool_size: 0,
			system_pool_size: 0,
			last_committed_timeslice: 0,
			last_timeslice: 1,
		};
		<CoretimeMarketImpl as MarketState>::set_status(status);

		// Status set but no sale info.
		assert!(matches!(place_order(1, 1, 100), Err(MarketError::NoSales)));

		let mut dummy_sale = SaleInfoRecord {
			sale_start: 0,
			leadin_length: 0,
			end_price: 200,
			sellout_price: None,
			region_begin: 0,
			region_end: 3,
			first_core: 3,
			ideal_cores_sold: 0,
			cores_offered: 1,
			cores_sold: 2,
		};
		<CoretimeMarketImpl as MarketState>::set_sale_info(dummy_sale.clone());

		// first_core >= core_count => Unavailable.
		assert!(matches!(place_order(1, 1, 100), Err(MarketError::Unavailable)));

		dummy_sale.first_core = 1;
		<CoretimeMarketImpl as MarketState>::set_sale_info(dummy_sale.clone());

		// cores_sold >= cores_offered => SoldOut.
		assert!(matches!(place_order(1, 1, 100), Err(MarketError::SoldOut)));

		// Start a proper sale to test TooEarly and Overpriced.
		start_sales(200, 1);

		// block_number == sale_start => TooEarly (needs block_number > sale_start).
		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		assert!(matches!(
			place_order(sale.sale_start, 1, 100),
			Err(MarketError::TooEarly)
		));

		// Price limit too low => Overpriced.
		assert!(matches!(
			place_order(sale.sale_start + 1, 1, 100),
			Err(MarketError::Overpriced)
		));
	});
}

#[test]
fn place_order_works() {
	TestExt::new().execute_with(|| {
		start_sales(100, 1);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let result = place_order_full(sale.sale_start + 1, 1, u64::MAX);
		assert!(result.is_ok());

		match result.unwrap() {
			OrderResult::Sold { price, region_id, region_end } => {
				assert!(price > 0);
				assert_eq!(region_id.begin, sale.region_begin);
				assert_eq!(region_id.mask, CoreMask::complete());
				assert_eq!(region_end, sale.region_end);
			},
			_ => panic!("Expected Sold"),
		}
	});
}

#[test]
fn renewal_order_price_capping() {
	let config = ConfigRecord {
		advance_notice: 2,
		interlude_length: 10,
		leadin_length: 20,
		ideal_bulk_proportion: Perbill::from_percent(100),
		limit_cores_offered: None,
		region_length: 20,
		renewal_bump: Perbill::from_percent(10),
		contribution_timeout: 5,
	};
	TestExt::new_with_config(config).execute_with(|| {
		start_sales(10, 2);
		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();

		// Place a renewal order with recorded_price = 100.
		// The price_cap = max(100 + 10% * 100, end_price) = max(110, end_price).
		// The next_renewal_price = min(current_price, price_cap).
		let result = place_renewal_order(sale.sale_start + 1, 1, 100);
		assert!(result.is_ok());

		match result.unwrap() {
			RenewalOrderResult::Sold { price, next_renewal_price, .. } => {
				// Price paid is the recorded price.
				assert_eq!(price, 100);
				// next_renewal_price is capped.
				let price_cap = 110u64.max(sale.end_price);
				let current_price =
					<CoretimeMarketImpl as MarketState>::current_price(sale.sale_start + 1)
						.unwrap();
				assert_eq!(next_renewal_price, current_price.min(price_cap));
			},
			_ => panic!("Expected Sold"),
		}
	});
}

#[test]
fn config_validation_works() {
	TestExt::new().execute_with(|| {
		let mut cfg = new_config();
		// Good config validates.
		assert!(cfg.validate().is_ok());

		// Bad config: leadin_length = 0.
		cfg.leadin_length = 0;
		assert!(cfg.validate().is_err());
	});
}

#[test]
fn sale_rotation_works() {
	TestExt::new().execute_with(|| {
		start_sales(100, 1);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		assert!(sale.cores_offered > 0);
		assert_eq!(sale.cores_sold, 0);
		assert_eq!(sale.end_price, 100);

		// Purchase one core.
		let result = place_order(sale.sale_start + 1, 1, u64::MAX);
		assert!(result.is_ok());

		let sale_after = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		assert_eq!(sale_after.cores_sold, 1);
	});
}

#[test]
fn sell_price_decreases_during_leadin() {
	let config = ConfigRecord {
		advance_notice: 2,
		interlude_length: 1,
		leadin_length: 10,
		ideal_bulk_proportion: Default::default(),
		limit_cores_offered: None,
		region_length: 3,
		renewal_bump: Perbill::from_percent(10),
		contribution_timeout: 5,
	};
	TestExt::new_with_config(config).execute_with(|| {
		start_sales(100, 1);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();

		// Price at start of leadin should be high.
		let price_early =
			<CoretimeMarketImpl as MarketState>::current_price(sale.sale_start + 1).unwrap();
		// Price later in leadin should be lower.
		let price_late =
			<CoretimeMarketImpl as MarketState>::current_price(sale.sale_start + 9).unwrap();
		// Price after leadin should be at end_price.
		let price_end =
			<CoretimeMarketImpl as MarketState>::current_price(sale.sale_start + 100).unwrap();

		assert!(price_early > price_late);
		assert!(price_late > price_end);
		assert_eq!(price_end, sale.end_price);
	});
}

#[test]
fn sold_out_prevents_further_purchases() {
	TestExt::new().execute_with(|| {
		start_sales(100, 1);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let block = sale.sale_start + 1;

		// Buy all offered cores.
		for _ in 0..sale.cores_offered {
			assert!(place_order(block, 1, u64::MAX).is_ok());
		}

		// Next purchase should fail with SoldOut.
		assert!(matches!(place_order(block, 1, u64::MAX), Err(MarketError::SoldOut)));
	});
}
