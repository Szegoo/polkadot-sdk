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
use frame_support::weights::WeightMeter;
use sp_arithmetic::Perbill;
use crate::SalePhase;
use sp_coretime::{
	ConfigRecord, CoreMask, Market, MarketError, MarketState, OrderResult, PotentialRenewalId,
	RenewalOrderResult, TickAction,
};
use sp_runtime::DispatchError;

type CoretimeMarketImpl = CoretimeMarket;

fn start_sales(reserve_price: u64, extra_cores: u16) {
	assert_ok!(CoretimeMarketImpl::start_sales(0, reserve_price, extra_cores));
}

fn tick(block_number: u64) -> Vec<TickAction<u64, u64, u64, u32>> {
	let mut meter = WeightMeter::new();
	CoretimeMarketImpl::tick(block_number, &mut meter)
}

fn place_bid(
	block_number: u64,
	who: u64,
	price_limit: u64,
) -> Result<OrderResult<u64, u32>, MarketError> {
	CoretimeMarketImpl::place_order(block_number, &who, price_limit)
}

fn place_renewal(
	block_number: u64,
	who: u64,
	core: u16,
	when: u32,
	recorded_price: u64,
) -> Result<RenewalOrderResult<u64, u32, u64>, MarketError> {
	let renewal_id = PotentialRenewalId { core, when };
	CoretimeMarketImpl::place_renewal_order(block_number, &who, renewal_id, recorded_price)
}

// ============================================================================
// Phase transition tests
// ============================================================================

#[test]
fn start_sales_initializes_market_phase() {
	TestExt::new().execute_with(|| {
		start_sales(100, 2);

		assert_eq!(crate::CurrentPhase::<Test>::get(), Some(SalePhase::Market));
		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		assert!(sale.cores_offered > 0);
		assert_eq!(sale.cores_sold, 0);
		assert_eq!(sale.clearing_price, None);
		assert!(sale.opening_price > 0);
		assert_eq!(sale.reserve_price, 100);
	});
}

#[test]
fn market_to_renewal_transition_on_timeout() {
	TestExt::new().execute_with(|| {
		start_sales(100, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let config = <CoretimeMarketImpl as MarketState>::configuration().unwrap();
		let market_end = sale.sale_start + config.market_period;

		// Before market end: still Market.
		tick(market_end - 1);
		assert_eq!(crate::CurrentPhase::<Test>::get(), Some(SalePhase::Market));

		// At market end: transitions to Renewal.
		tick(market_end);
		assert_eq!(crate::CurrentPhase::<Test>::get(), Some(SalePhase::Renewal));
	});
}

#[test]
fn renewal_to_settlement_transition() {
	TestExt::new().execute_with(|| {
		start_sales(100, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let config = <CoretimeMarketImpl as MarketState>::configuration().unwrap();
		let market_end = sale.sale_start + config.market_period;
		let renewal_end = market_end + config.renewal_period;

		tick(market_end);
		assert_eq!(crate::CurrentPhase::<Test>::get(), Some(SalePhase::Renewal));

		tick(renewal_end - 1);
		assert_eq!(crate::CurrentPhase::<Test>::get(), Some(SalePhase::Renewal));

		tick(renewal_end);
		assert_eq!(crate::CurrentPhase::<Test>::get(), Some(SalePhase::Settlement));
	});
}

#[test]
fn settlement_to_market_transition_on_rotation() {
	TestExt::new().execute_with(|| {
		start_sales(100, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let config = <CoretimeMarketImpl as MarketState>::configuration().unwrap();
		let market_end = sale.sale_start + config.market_period;
		let renewal_end = market_end + config.renewal_period;

		tick(market_end);
		tick(renewal_end);
		assert_eq!(crate::CurrentPhase::<Test>::get(), Some(SalePhase::Settlement));

		// Set last_committed_timeslice >= region_begin to trigger rotation.
		let mut status = <CoretimeMarketImpl as MarketState>::status().unwrap();
		status.last_committed_timeslice = sale.region_begin;
		<CoretimeMarketImpl as MarketState>::set_status(status);

		let actions = tick(renewal_end + 1);
		assert_eq!(crate::CurrentPhase::<Test>::get(), Some(SalePhase::Market));
		assert!(actions.iter().any(|a| matches!(a, TickAction::SaleRotated { .. })));
	});
}

// ============================================================================
// Bidding tests
// ============================================================================

#[test]
fn place_bid_works_during_market_phase() {
	TestExt::new().execute_with(|| {
		start_sales(100, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let block = sale.sale_start + 1;
		let current_price =
			<CoretimeMarketImpl as MarketState>::current_price(block).unwrap();

		let result = place_bid(block, 1, current_price);
		assert!(result.is_ok());
		match result.unwrap() {
			OrderResult::BidPlaced { id, bid_price } => {
				assert_eq!(id, 0);
				assert_eq!(bid_price, current_price);
			},
			_ => panic!("Expected BidPlaced"),
		}
	});
}

#[test]
fn place_bid_fails_before_sale_start() {
	TestExt::new().execute_with(|| {
		// Start at block 1 so there's a block before sale_start to test with.
		assert_ok!(CoretimeMarketImpl::start_sales(1, 100, 2));

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		assert!(matches!(
			place_bid(sale.sale_start - 1, 1, 100),
			Err(MarketError::TooEarly)
		));
	});
}

#[test]
fn place_bid_clamps_to_current_price() {
	TestExt::new().execute_with(|| {
		start_sales(100, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let block = sale.sale_start + 1;
		let current_price =
			<CoretimeMarketImpl as MarketState>::current_price(block).unwrap();

		// Bidding above current price should clamp to current price, not fail.
		let result = place_bid(block, 1, current_price + 1).unwrap();
		match result {
			OrderResult::BidPlaced { bid_price, .. } => {
				assert_eq!(bid_price, current_price);
			},
			_ => panic!("Expected BidPlaced"),
		}
	});
}

#[test]
fn place_bid_fails_during_renewal_phase() {
	TestExt::new().execute_with(|| {
		start_sales(100, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let config = <CoretimeMarketImpl as MarketState>::configuration().unwrap();
		let market_end = sale.sale_start + config.market_period;

		tick(market_end);

		assert!(matches!(
			place_bid(market_end + 1, 1, 100),
			Err(MarketError::WrongPhase)
		));
	});
}

#[test]
fn place_bid_fails_without_sale_info() {
	TestExt::new().execute_with(|| {
		// Before sales are started, CurrentPhase is None => WrongPhase.
		assert!(matches!(place_bid(1, 1, 100), Err(MarketError::WrongPhase)));
	});
}

#[test]
fn place_bid_enforces_max_bids() {
	TestExt::new().execute_with(|| {
		start_sales(100, 200);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let block = sale.sale_start + 1;
		let current_price =
			<CoretimeMarketImpl as MarketState>::current_price(block).unwrap();

		// Place MaxBids (100) bids.
		for i in 0..100u64 {
			assert!(place_bid(block, i, current_price).is_ok());
		}

		// Next bid should fail — MaxBids reached.
		assert!(matches!(
			place_bid(block, 1, current_price),
			Err(MarketError::SoldOut)
		));
	});
}

// ============================================================================
// Clearing price / auction settlement tests
// ============================================================================

#[test]
fn clearing_price_is_kth_highest_bid() {
	TestExt::new().execute_with(|| {
		start_sales(10, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let block = sale.sale_start + 1;
		let current_price =
			<CoretimeMarketImpl as MarketState>::current_price(block).unwrap();

		let high_bid = current_price;
		let mid_bid = current_price / 2;
		let low_bid = current_price / 4;

		assert!(place_bid(block, 1, high_bid).is_ok());
		assert!(place_bid(block, 2, mid_bid).is_ok());
		assert!(place_bid(block, 3, low_bid).is_ok());

		let config = <CoretimeMarketImpl as MarketState>::configuration().unwrap();
		let market_end = sale.sale_start + config.market_period;

		let actions = tick(market_end);

		// Clearing price = max(2nd highest bid, reserve).
		let clearing = crate::AuctionClearingPrice::<Test>::get().unwrap();
		assert_eq!(clearing, mid_bid.max(sale.reserve_price));

		// Bidder 1 (high_bid) wins and gets refund of excess.
		let excess = high_bid - clearing;
		assert!(actions
			.iter()
			.any(|a| matches!(a, TickAction::Refund { amount, who } if *who == 1 && *amount == excess)));

		// Bidder 2 (mid_bid) loses and gets full refund.
		let excess = mid_bid - clearing;
		assert!(actions
			.iter()
			.any(|a| matches!(a, TickAction::Refund { amount, who } if *who == 3 && *amount == low_bid)));

		// Bidder 3 (low_bid < clearing) loses and gets full refund.
		assert!(actions
			.iter()
			.any(|a| matches!(a, TickAction::Refund { amount, who } if *who == 3 && *amount == low_bid)));
	});
}

#[test]
fn clearing_price_falls_back_to_reserve_when_undersubscribed() {
	TestExt::new().execute_with(|| {
		start_sales(10, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let block = sale.sale_start + 1;
		let current_price =
			<CoretimeMarketImpl as MarketState>::current_price(block).unwrap();

		assert!(place_bid(block, 1, current_price).is_ok());

		let config = <CoretimeMarketImpl as MarketState>::configuration().unwrap();
		tick(sale.sale_start + config.market_period);

		let clearing = crate::AuctionClearingPrice::<Test>::get().unwrap();
		assert_eq!(clearing, sale.reserve_price);
	});
}

#[test]
fn no_bids_results_in_reserve_clearing_price() {
	TestExt::new().execute_with(|| {
		start_sales(50, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let config = <CoretimeMarketImpl as MarketState>::configuration().unwrap();

		tick(sale.sale_start + config.market_period);

		let clearing = crate::AuctionClearingPrice::<Test>::get().unwrap();
		assert_eq!(clearing, sale.reserve_price);
		assert!(crate::Allocations::<Test>::get().is_empty());
	});
}

#[test]
fn winners_pay_clearing_price_not_bid_price() {
	TestExt::new().execute_with(|| {
		start_sales(10, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let block = sale.sale_start + 1;
		let price = <CoretimeMarketImpl as MarketState>::current_price(block).unwrap();

		assert!(place_bid(block, 1, price).is_ok());
		assert!(place_bid(block, 2, price).is_ok());

		let config = <CoretimeMarketImpl as MarketState>::configuration().unwrap();
		tick(sale.sale_start + config.market_period);

		let allocations = crate::Allocations::<Test>::get();
		assert_eq!(allocations.len(), 2);
		for alloc in &allocations {
			assert_eq!(alloc.clearing_price, price.max(sale.reserve_price));
		}
	});
}

// ============================================================================
// Region issuance tests
// ============================================================================

#[test]
fn regions_issued_at_renewal_end() {
	TestExt::new().execute_with(|| {
		start_sales(10, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let block = sale.sale_start + 1;
		let price = <CoretimeMarketImpl as MarketState>::current_price(block).unwrap();

		assert!(place_bid(block, 1, price).is_ok());
		assert!(place_bid(block, 2, price).is_ok());

		let config = <CoretimeMarketImpl as MarketState>::configuration().unwrap();
		let market_end = sale.sale_start + config.market_period;
		let renewal_end = market_end + config.renewal_period;

		tick(market_end);

		let actions = tick(renewal_end);

		let sell_regions: Vec<_> = actions
			.iter()
			.filter(|a| matches!(a, TickAction::SellRegion { .. }))
			.collect();
		assert_eq!(sell_regions.len(), 2);

		for action in &sell_regions {
			if let TickAction::SellRegion { paid, region_id, region_end, .. } = action {
				assert_eq!(*paid, price.max(sale.reserve_price));
				assert_eq!(region_id.begin, sale.region_begin);
				assert_eq!(region_id.mask, CoreMask::complete());
				assert_eq!(*region_end, sale.region_end);
			}
		}
	});
}

// ============================================================================
// Renewal tests
// ============================================================================

#[test]
fn renewal_during_market_phase_fails() {
	TestExt::new().execute_with(|| {
		start_sales(100, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let block = sale.sale_start + 1;

		// Renewals are not allowed during Market phase — use place_order instead.
		let result = place_renewal(block, 1, 0, sale.region_begin, 500);
		assert!(matches!(result, Err(MarketError::WrongPhase)));
	});
}

#[test]
fn renewal_during_renewal_phase_gets_core() {
	TestExt::new().execute_with(|| {
		start_sales(10, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let config = <CoretimeMarketImpl as MarketState>::configuration().unwrap();
		let market_end = sale.sale_start + config.market_period;

		// Only 1 bid out of 2 cores — undersubscribed.
		let block = sale.sale_start + 1;
		let price = <CoretimeMarketImpl as MarketState>::current_price(block).unwrap();
		assert!(place_bid(block, 1, price).is_ok());

		tick(market_end);

		let result = place_renewal(market_end + 1, 2, 0, sale.region_begin, 100);
		assert!(result.is_ok());
		match result.unwrap() {
			RenewalOrderResult::Sold { region_id, displaced, .. } => {
				assert_eq!(region_id.begin, sale.region_begin);
				assert_eq!(region_id.mask, CoreMask::complete());
				assert!(displaced.is_none());
			},
			_ => panic!("Expected Sold during Renewal phase"),
		}
	});
}

#[test]
fn renewal_with_displacement() {
	TestExt::new().execute_with(|| {
		start_sales(10, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let config = <CoretimeMarketImpl as MarketState>::configuration().unwrap();
		let market_end = sale.sale_start + config.market_period;

		let block = sale.sale_start + 1;
		let price = <CoretimeMarketImpl as MarketState>::current_price(block).unwrap();
		assert!(place_bid(block, 10, price).is_ok());
		assert!(place_bid(block, 20, price).is_ok());

		tick(market_end);

		let allocations = crate::Allocations::<Test>::get();
		assert_eq!(allocations.len(), 2);

		let result = place_renewal(market_end + 1, 30, 0, sale.region_begin, 100);
		assert!(result.is_ok());
		match result.unwrap() {
			RenewalOrderResult::Sold { displaced, .. } => {
				assert!(displaced.is_some());
				let d = displaced.unwrap();
				assert!(d.who == 10 || d.who == 20);
				// Displaced winner gets clearing_price refunded (excess was already refunded).
				let clearing = crate::AuctionClearingPrice::<Test>::get().unwrap();
				assert_eq!(d.refund, clearing);
			},
			_ => panic!("Expected Sold with displacement"),
		}

		let allocations = crate::Allocations::<Test>::get();
		assert_eq!(allocations.len(), 1);
	});
}

#[test]
fn renewal_displacement_protects_renewers_with_rights() {
	TestExt::new().execute_with(|| {
		start_sales(10, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let config = <CoretimeMarketImpl as MarketState>::configuration().unwrap();
		let market_end = sale.sale_start + config.market_period;

		let block = sale.sale_start + 1;
		let price = <CoretimeMarketImpl as MarketState>::current_price(block).unwrap();

		TestRenewalRights::set(10, sale.region_end, 1);

		assert!(place_bid(block, 10, price).is_ok());
		assert!(place_bid(block, 20, price).is_ok());

		tick(market_end);

		let allocations = crate::Allocations::<Test>::get();
		let bidder_10 = allocations.iter().find(|a| a.who == 10).unwrap();
		assert!(bidder_10.has_renewal_rights);
		let bidder_20 = allocations.iter().find(|a| a.who == 20).unwrap();
		assert!(!bidder_20.has_renewal_rights);

		let result = place_renewal(market_end + 1, 30, 0, sale.region_begin, 100);
		assert!(result.is_ok());
		match result.unwrap() {
			RenewalOrderResult::Sold { displaced, .. } => {
				let d = displaced.unwrap();
				assert_eq!(d.who, 20);
			},
			_ => panic!("Expected Sold"),
		}
	});
}

#[test]
fn renewal_fails_when_all_winners_have_renewal_rights() {
	TestExt::new().execute_with(|| {
		start_sales(10, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let config = <CoretimeMarketImpl as MarketState>::configuration().unwrap();
		let market_end = sale.sale_start + config.market_period;

		let block = sale.sale_start + 1;
		let price = <CoretimeMarketImpl as MarketState>::current_price(block).unwrap();

		TestRenewalRights::set(10, sale.region_end, 1);
		TestRenewalRights::set(20, sale.region_end, 1);

		assert!(place_bid(block, 10, price).is_ok());
		assert!(place_bid(block, 20, price).is_ok());

		tick(market_end);

		let result = place_renewal(market_end + 1, 30, 0, sale.region_begin, 100);
		assert!(matches!(result, Err(MarketError::Unavailable)));
	});
}

#[test]
fn renewal_penalty_applied_when_oversubscribed() {
	TestExt::new().execute_with(|| {
		start_sales(10, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let config = <CoretimeMarketImpl as MarketState>::configuration().unwrap();
		let market_end = sale.sale_start + config.market_period;

		let block = sale.sale_start + 1;
		let price = <CoretimeMarketImpl as MarketState>::current_price(block).unwrap();

		assert!(place_bid(block, 10, price).is_ok());
		assert!(place_bid(block, 20, price).is_ok());

		tick(market_end);

		let clearing = crate::AuctionClearingPrice::<Test>::get().unwrap();
		let penalty = config.penalty * clearing;
		let expected_renewal_price = clearing + penalty;

		let result = place_renewal(market_end + 1, 30, 0, sale.region_begin, 100);
		assert!(result.is_ok());
		match result.unwrap() {
			RenewalOrderResult::Sold { price, .. } => {
				assert_eq!(price, expected_renewal_price);
			},
			_ => panic!("Expected Sold"),
		}
	});
}

#[test]
fn renewal_no_penalty_when_undersubscribed() {
	TestExt::new().execute_with(|| {
		start_sales(10, 3);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let config = <CoretimeMarketImpl as MarketState>::configuration().unwrap();
		let market_end = sale.sale_start + config.market_period;

		let block = sale.sale_start + 1;
		let price = <CoretimeMarketImpl as MarketState>::current_price(block).unwrap();

		assert!(place_bid(block, 10, price).is_ok());

		tick(market_end);

		let clearing = crate::AuctionClearingPrice::<Test>::get().unwrap();

		let result = place_renewal(market_end + 1, 30, 0, sale.region_begin, 100);
		assert!(result.is_ok());
		match result.unwrap() {
			RenewalOrderResult::Sold { price, .. } => {
				assert_eq!(price, clearing);
			},
			_ => panic!("Expected Sold"),
		}
	});
}

#[test]
fn renewal_fails_during_settlement() {
	TestExt::new().execute_with(|| {
		start_sales(10, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let config = <CoretimeMarketImpl as MarketState>::configuration().unwrap();
		let market_end = sale.sale_start + config.market_period;
		let renewal_end = market_end + config.renewal_period;

		tick(market_end);
		tick(renewal_end);

		let result = place_renewal(renewal_end + 1, 1, 0, sale.region_begin, 100);
		assert!(matches!(result, Err(MarketError::WrongPhase)));
	});
}

// ============================================================================
// raise_bid tests
// ============================================================================

#[test]
fn raise_bid_works() {
	TestExt::new().execute_with(|| {
		start_sales(100, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let block = sale.sale_start + 1;
		let price = <CoretimeMarketImpl as MarketState>::current_price(block).unwrap();

		let initial_bid = price / 2;
		let result = place_bid(block, 1, initial_bid).unwrap();
		let bid_id = match result {
			OrderResult::BidPlaced { id, .. } => id,
			_ => panic!("Expected BidPlaced"),
		};

		let new_price = price;
		let additional =
			CoretimeMarketImpl::raise_bid(block, bid_id, &1, new_price).unwrap();
		assert_eq!(additional, new_price - initial_bid);

		let bid = crate::Bids::<Test>::get(bid_id).unwrap();
		assert_eq!(bid.price, new_price);
	});
}

#[test]
fn raise_bid_fails_for_wrong_owner() {
	TestExt::new().execute_with(|| {
		start_sales(100, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let block = sale.sale_start + 1;
		let price = <CoretimeMarketImpl as MarketState>::current_price(block).unwrap();

		let result = place_bid(block, 1, price / 2).unwrap();
		let bid_id = match result {
			OrderResult::BidPlaced { id, .. } => id,
			_ => panic!("Expected BidPlaced"),
		};

		assert!(matches!(
			CoretimeMarketImpl::raise_bid(block, bid_id, &2, price),
			Err(MarketError::BidNotExist)
		));
	});
}

#[test]
fn raise_bid_fails_for_lower_price() {
	TestExt::new().execute_with(|| {
		start_sales(100, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let block = sale.sale_start + 1;
		let price = <CoretimeMarketImpl as MarketState>::current_price(block).unwrap();

		let result = place_bid(block, 1, price).unwrap();
		let bid_id = match result {
			OrderResult::BidPlaced { id, .. } => id,
			_ => panic!("Expected BidPlaced"),
		};

		assert!(matches!(
			CoretimeMarketImpl::raise_bid(block, bid_id, &1, price / 2),
			Err(MarketError::Overpriced)
		));
	});
}

#[test]
fn raise_bid_fails_above_current_price() {
	TestExt::new().execute_with(|| {
		start_sales(100, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let block = sale.sale_start + 1;
		let price = <CoretimeMarketImpl as MarketState>::current_price(block).unwrap();

		let result = place_bid(block, 1, price / 3).unwrap();
		let bid_id = match result {
			OrderResult::BidPlaced { id, .. } => id,
			_ => panic!("Expected BidPlaced"),
		};

		// Try to raise above current descending price.
		assert!(matches!(
			CoretimeMarketImpl::raise_bid(block, bid_id, &1, price + 1),
			Err(MarketError::BidTooHigh)
		));
	});
}

#[test]
fn raise_bid_fails_during_renewal_phase() {
	TestExt::new().execute_with(|| {
		start_sales(100, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let block = sale.sale_start + 1;
		let price = <CoretimeMarketImpl as MarketState>::current_price(block).unwrap();

		let result = place_bid(block, 1, price / 2).unwrap();
		let bid_id = match result {
			OrderResult::BidPlaced { id, .. } => id,
			_ => panic!("Expected BidPlaced"),
		};

		let config = <CoretimeMarketImpl as MarketState>::configuration().unwrap();
		tick(sale.sale_start + config.market_period);

		assert!(matches!(
			CoretimeMarketImpl::raise_bid(block, bid_id, &1, price),
			Err(MarketError::WrongPhase)
		));
	});
}

// ============================================================================
// close_bid tests
// ============================================================================

#[test]
fn close_bid_always_fails() {
	TestExt::new().execute_with(|| {
		start_sales(100, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let block = sale.sale_start + 1;
		let price = <CoretimeMarketImpl as MarketState>::current_price(block).unwrap();

		let result = place_bid(block, 1, price).unwrap();
		let bid_id = match result {
			OrderResult::BidPlaced { id, .. } => id,
			_ => panic!("Expected BidPlaced"),
		};

		assert!(matches!(
			CoretimeMarketImpl::close_bid(bid_id, Some(1)),
			Err(MarketError::BidNotCancellable)
		));
	});
}

// ============================================================================
// Descending price tests
// ============================================================================

#[test]
fn price_descends_linearly_during_market_phase() {
	TestExt::new().execute_with(|| {
		start_sales(100, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let config = <CoretimeMarketImpl as MarketState>::configuration().unwrap();

		let price_start =
			<CoretimeMarketImpl as MarketState>::current_price(sale.sale_start + 1).unwrap();
		let price_mid = <CoretimeMarketImpl as MarketState>::current_price(
			sale.sale_start + config.market_period / 2,
		)
		.unwrap();
		let price_end = <CoretimeMarketImpl as MarketState>::current_price(
			sale.sale_start + config.market_period,
		)
		.unwrap();

		assert!(price_start > price_mid);
		assert!(price_mid > price_end);
		assert_eq!(price_end, sale.reserve_price);
	});
}

// ============================================================================
// Config validation tests
// ============================================================================

#[test]
fn config_validation_works() {
	TestExt::new().execute_with(|| {
		let mut cfg = new_config();
		assert!(cfg.validate().is_ok());

		cfg.market_period = 0;
		assert!(cfg.validate().is_err());
	});
}

// ============================================================================
// Sale rotation tests
// ============================================================================

#[test]
fn sale_rotation_creates_new_sale_with_correct_parameters() {
	TestExt::new().execute_with(|| {
		start_sales(100, 2);

		let sale1 = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let config = <CoretimeMarketImpl as MarketState>::configuration().unwrap();
		let market_end = sale1.sale_start + config.market_period;
		let renewal_end = market_end + config.renewal_period;

		tick(market_end);
		tick(renewal_end);

		let mut status = <CoretimeMarketImpl as MarketState>::status().unwrap();
		status.last_committed_timeslice = sale1.region_begin;
		<CoretimeMarketImpl as MarketState>::set_status(status);

		let rotation_block = renewal_end + 1;
		tick(rotation_block);

		let sale2 = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		assert_eq!(sale2.region_begin, sale1.region_end);
		assert_eq!(sale2.region_end, sale1.region_end + config.region_length);
		assert_eq!(sale2.clearing_price, None);
		assert_eq!(sale2.cores_sold, 0);
		assert_eq!(sale2.sale_start, rotation_block);
	});
}

#[test]
fn sale_rotation_cleans_up_previous_state() {
	TestExt::new().execute_with(|| {
		start_sales(100, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let config = <CoretimeMarketImpl as MarketState>::configuration().unwrap();
		let block = sale.sale_start + 1;
		let price = <CoretimeMarketImpl as MarketState>::current_price(block).unwrap();

		assert!(place_bid(block, 1, price).is_ok());

		let market_end = sale.sale_start + config.market_period;
		let renewal_end = market_end + config.renewal_period;

		tick(market_end);
		tick(renewal_end);

		// At this point we have a clearing price and NextBidId > 0.
		assert!(crate::AuctionClearingPrice::<Test>::get().is_some());
		assert!(crate::NextBidId::<Test>::get() > 0);

		let mut status = <CoretimeMarketImpl as MarketState>::status().unwrap();
		status.last_committed_timeslice = sale.region_begin;
		<CoretimeMarketImpl as MarketState>::set_status(status);

		tick(renewal_end + 1);

		// After rotation, previous sale state is cleaned up.
		assert!(crate::AuctionClearingPrice::<Test>::get().is_none());
		assert_eq!(crate::NextBidId::<Test>::get(), 0);
	});
}

// ============================================================================
// Full lifecycle test
// ============================================================================

#[test]
fn full_sale_lifecycle() {
	let config = ConfigRecord {
		advance_notice: 2,
		market_period: 100,
		renewal_period: 10,
		ideal_bulk_proportion: Perbill::from_percent(100),
		limit_cores_offered: None,
		region_length: 3,
		penalty: Perbill::from_percent(30),
		contribution_timeout: 5,
	};
	TestExt::new_with_config(config.clone()).execute_with(|| {
		start_sales(100, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let block = sale.sale_start + 1;
		let current_price =
			<CoretimeMarketImpl as MarketState>::current_price(block).unwrap();

		// --- Market Phase: 3 bidders compete for 2 cores ---
		let bid1 = current_price;
		let bid2 = current_price - 1;
		let bid3 = current_price - 2;

		assert!(place_bid(block, 1, bid1).is_ok());
		assert!(place_bid(block, 2, bid2).is_ok());
		assert!(place_bid(block, 3, bid3).is_ok());

		// --- Auction settles ---
		let market_end = sale.sale_start + config.market_period;
		let settle_actions = tick(market_end);

		let clearing = crate::AuctionClearingPrice::<Test>::get().unwrap();
		assert_eq!(clearing, bid2.max(sale.reserve_price));

		// 2 winners, 1 loser.
		let allocations = crate::Allocations::<Test>::get();
		assert_eq!(allocations.len(), 2);

		let refund_count = settle_actions
			.iter()
			.filter(|a| matches!(a, TickAction::Refund { .. }))
			.count();
		assert!(refund_count >= 1);

		// --- Renewal Phase: renewer displaces an auction winner ---
		let result = place_renewal(market_end + 1, 100, 0, sale.region_begin, 500);
		assert!(result.is_ok());
		match result.unwrap() {
			RenewalOrderResult::Sold { displaced, price: renewal_price, .. } => {
				assert!(displaced.is_some());
				let penalty = config.penalty * clearing;
				assert_eq!(renewal_price, clearing + penalty);
				// Displaced gets clearing_price refund.
				assert_eq!(displaced.unwrap().refund, clearing);
			},
			_ => panic!("Expected Sold with displacement"),
		}

		// --- Renewal ends: regions issued for remaining allocation ---
		let renewal_end = market_end + config.renewal_period;
		let finalize_actions = tick(renewal_end);

		let region_count = finalize_actions
			.iter()
			.filter(|a| matches!(a, TickAction::SellRegion { .. }))
			.count();
		assert_eq!(region_count, 1);

		assert_eq!(crate::CurrentPhase::<Test>::get(), Some(SalePhase::Settlement));

		// --- Settlement => Next sale ---
		let mut status = <CoretimeMarketImpl as MarketState>::status().unwrap();
		status.last_committed_timeslice = sale.region_begin;
		<CoretimeMarketImpl as MarketState>::set_status(status);

		tick(renewal_end + 1);
		assert_eq!(crate::CurrentPhase::<Test>::get(), Some(SalePhase::Market));

		let sale2 = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		assert_eq!(sale2.region_begin, sale.region_end);
	});
}

// ============================================================================
// MarketState tests
// ============================================================================

#[test]
fn market_state_current_price_returns_none_without_sale() {
	TestExt::new().execute_with(|| {
		assert_eq!(<CoretimeMarketImpl as MarketState>::current_price(1), None);
	});
}

#[test]
fn market_state_returns_clearing_price_after_settlement() {
	TestExt::new().execute_with(|| {
		start_sales(50, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let config = <CoretimeMarketImpl as MarketState>::configuration().unwrap();
		let market_end = sale.sale_start + config.market_period;

		let block = sale.sale_start + 1;
		let price = <CoretimeMarketImpl as MarketState>::current_price(block).unwrap();
		assert!(place_bid(block, 1, price).is_ok());

		tick(market_end);

		let clearing = crate::AuctionClearingPrice::<Test>::get().unwrap();
		let reported_price =
			<CoretimeMarketImpl as MarketState>::current_price(market_end + 1).unwrap();
		assert_eq!(reported_price, clearing);
	});
}

// ============================================================================
// Event tests
// ============================================================================

#[test]
fn events_emitted_on_bid_placed() {
	TestExt::new().execute_with(|| {
		System::set_block_number(1);
		start_sales(100, 2);
		System::reset_events();

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let block = sale.sale_start + 1;
		let price = <CoretimeMarketImpl as MarketState>::current_price(block).unwrap();

		assert!(place_bid(block, 1, price).is_ok());

		let events = System::events();
		assert!(events.iter().any(|e| matches!(
			&e.event,
			RuntimeEvent::CoretimeMarket(crate::Event::BidPlaced {
				who: 1,
				bid_id: 0,
				..
			})
		)));
	});
}

#[test]
fn events_emitted_on_phase_transitions() {
	TestExt::new().execute_with(|| {
		System::set_block_number(1);
		start_sales(100, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let config = <CoretimeMarketImpl as MarketState>::configuration().unwrap();
		let market_end = sale.sale_start + config.market_period;

		System::reset_events();
		tick(market_end);

		let events = System::events();
		assert!(events.iter().any(|e| matches!(
			&e.event,
			RuntimeEvent::CoretimeMarket(crate::Event::PhaseTransitioned {
				from: SalePhase::Market,
				to: SalePhase::Renewal,
			})
		)));
		assert!(events.iter().any(|e| matches!(
			&e.event,
			RuntimeEvent::CoretimeMarket(crate::Event::AuctionSettled { .. })
		)));
	});
}

#[test]
fn events_emitted_on_bid_raised() {
	TestExt::new().execute_with(|| {
		System::set_block_number(1);
		start_sales(100, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let block = sale.sale_start + 1;
		let price = <CoretimeMarketImpl as MarketState>::current_price(block).unwrap();

		let bid_id = match place_bid(block, 1, price / 2).unwrap() {
			OrderResult::BidPlaced { id, .. } => id,
			_ => panic!("Expected BidPlaced"),
		};

		System::reset_events();
		assert!(CoretimeMarketImpl::raise_bid(block, bid_id, &1, price).is_ok());

		let events = System::events();
		assert!(events.iter().any(|e| matches!(
			&e.event,
			RuntimeEvent::CoretimeMarket(crate::Event::BidRaised {
				who: 1,
				bid_id: 0,
				..
			})
		)));
	});
}

#[test]
fn events_emitted_on_displacement() {
	TestExt::new().execute_with(|| {
		System::set_block_number(1);
		start_sales(10, 2);

		let sale = <CoretimeMarketImpl as MarketState>::sale_info().unwrap();
		let config = <CoretimeMarketImpl as MarketState>::configuration().unwrap();
		let market_end = sale.sale_start + config.market_period;

		let block = sale.sale_start + 1;
		let price = <CoretimeMarketImpl as MarketState>::current_price(block).unwrap();

		assert!(place_bid(block, 10, price).is_ok());
		assert!(place_bid(block, 20, price).is_ok());

		tick(market_end);
		System::reset_events();

		assert!(place_renewal(market_end + 1, 30, 0, sale.region_begin, 100).is_ok());

		let events = System::events();
		assert!(events
			.iter()
			.any(|e| matches!(
				&e.event,
				RuntimeEvent::CoretimeMarket(crate::Event::BidDisplaced { .. })
			)));
		assert!(events
			.iter()
			.any(|e| matches!(
				&e.event,
				RuntimeEvent::CoretimeMarket(crate::Event::RenewalExercised { who: 30, .. })
			)));
	});
}
