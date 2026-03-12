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

//! Coretime primitives: shared types and traits for the coretime market and broker pallets.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::vec::Vec;
use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use core::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign, BitXor, BitXorAssign, Not};
use scale_info::TypeInfo;
use sp_arithmetic::Perbill;
use sp_runtime::{DispatchError, FixedPointNumber, FixedPointOperand, FixedU64};
use sp_weights::WeightMeter;

/// Index of a Polkadot Core.
pub type CoreIndex = u16;

/// A Task Id. In general this is called a ParachainId.
pub type TaskId = u32;

/// Relay-chain block number with a fixed divisor of TimeslicePeriod.
pub type Timeslice = u32;

/// Counter for the total number of set bits over every core's `CoreMask`.
pub type CoreMaskBitCount = u32;

/// The same as `CoreMaskBitCount` but signed.
pub type SignedCoreMaskBitCount = i32;

/// Fraction expressed as a nominator with an assumed denominator of 57,600.
pub type PartsOf57600 = u16;

/// The number of bits in the `CoreMask`.
pub const CORE_MASK_BITS: usize = 80;

#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	Default,
	Copy,
	Clone,
	PartialEq,
	Eq,
	Debug,
	TypeInfo,
	MaxEncodedLen,
)]
pub struct CoreMask([u8; 10]);

impl CoreMask {
	pub fn void() -> Self {
		Self([0u8; 10])
	}
	pub fn complete() -> Self {
		Self([255u8; 10])
	}
	pub fn is_void(&self) -> bool {
		&self.0 == &[0u8; 10]
	}
	pub fn is_complete(&self) -> bool {
		&self.0 == &[255u8; 10]
	}
	pub fn set(&mut self, i: u32) -> Self {
		if i < 80 {
			self.0[(i / 8) as usize] |= 128 >> (i % 8);
		}
		*self
	}
	pub fn clear(&mut self, i: u32) -> Self {
		if i < 80 {
			self.0[(i / 8) as usize] &= !(128 >> (i % 8));
		}
		*self
	}
	pub fn count_zeros(&self) -> u32 {
		self.0.iter().map(|i| i.count_zeros()).sum()
	}
	pub fn count_ones(&self) -> u32 {
		self.0.iter().map(|i| i.count_ones()).sum()
	}
	pub fn from_chunk(from: u32, to: u32) -> Self {
		let mut v = [0u8; 10];
		for i in (from.min(80) as usize)..(to.min(80) as usize) {
			v[i / 8] |= 128 >> (i % 8);
		}
		Self(v)
	}
}

impl From<u128> for CoreMask {
	fn from(x: u128) -> Self {
		let mut v = [0u8; 10];
		v.iter_mut().rev().fold(x, |a, i| {
			*i = a as u8;
			a >> 8
		});
		Self(v)
	}
}
impl From<CoreMask> for u128 {
	fn from(x: CoreMask) -> Self {
		x.0.into_iter().fold(0u128, |a, i| (a << 8) | i as u128)
	}
}
impl BitAnd<Self> for CoreMask {
	type Output = Self;
	fn bitand(mut self, rhs: Self) -> Self {
		self.bitand_assign(rhs);
		self
	}
}
impl BitAndAssign<Self> for CoreMask {
	fn bitand_assign(&mut self, rhs: Self) {
		for i in 0..10 {
			self.0[i].bitand_assign(rhs.0[i]);
		}
	}
}
impl BitOr<Self> for CoreMask {
	type Output = Self;
	fn bitor(mut self, rhs: Self) -> Self {
		self.bitor_assign(rhs);
		self
	}
}
impl BitOrAssign<Self> for CoreMask {
	fn bitor_assign(&mut self, rhs: Self) {
		for i in 0..10 {
			self.0[i].bitor_assign(rhs.0[i]);
		}
	}
}
impl BitXor<Self> for CoreMask {
	type Output = Self;
	fn bitxor(mut self, rhs: Self) -> Self {
		self.bitxor_assign(rhs);
		self
	}
}
impl BitXorAssign<Self> for CoreMask {
	fn bitxor_assign(&mut self, rhs: Self) {
		for i in 0..10 {
			self.0[i].bitxor_assign(rhs.0[i]);
		}
	}
}
impl Not for CoreMask {
	type Output = Self;
	fn not(self) -> Self {
		let mut result = [0u8; 10];
		for i in 0..10 {
			result[i] = self.0[i].not();
		}
		Self(result)
	}
}

/// An element to which a core can be assigned.
#[derive(
	Encode,
	Decode,
	DecodeWithMemTracking,
	Clone,
	Eq,
	PartialEq,
	Ord,
	PartialOrd,
	Debug,
	TypeInfo,
	MaxEncodedLen,
)]
pub enum CoreAssignment {
	/// Core need not be used for anything.
	Idle,
	/// Core should be used for the Instantaneous Coretime Pool.
	Pool,
	/// Core should be used to process the given task.
	Task(TaskId),
}

/// Self-describing identity for a Region of Bulk Coretime.
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
pub struct RegionId {
	/// The timeslice at which this Region begins.
	pub begin: Timeslice,
	/// The index of the Polkadot Core on which this Region will be scheduled.
	pub core: CoreIndex,
	/// The regularity parts in which this Region will be scheduled.
	pub mask: CoreMask,
}

impl From<u128> for RegionId {
	fn from(x: u128) -> Self {
		Self { begin: (x >> 96) as u32, core: (x >> 80) as u16, mask: x.into() }
	}
}

impl From<RegionId> for u128 {
	fn from(x: RegionId) -> Self {
		((x.begin as u128) << 96) | ((x.core as u128) << 80) | u128::from(x.mask)
	}
}

/// The identity of a possibly renewable Core workload.
#[derive(Encode, Decode, Copy, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen)]
pub struct PotentialRenewalId {
	/// The core whose workload at the sale ending with `when` may be renewed to begin at `when`.
	pub core: CoreIndex,
	/// The point in time that the renewable workload on `core` ends and a fresh renewal may begin.
	pub when: Timeslice,
}

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

/// The status of a Bulk Coretime Sale.
#[derive(Encode, Decode, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen)]
pub struct SaleInfoRecord<Balance, BlockNumber> {
	/// The relay block number at which the sale will/did start.
	pub sale_start: BlockNumber,
	/// The length in blocks of the Leadin Period (where the price is decreasing).
	pub leadin_length: BlockNumber,
	/// The price of Bulk Coretime after the Leadin Period.
	pub end_price: Balance,
	/// The first timeslice of the Regions which are being sold in this sale.
	pub region_begin: Timeslice,
	/// The timeslice on which the Regions which are being sold in the sale terminate. (i.e. One
	/// after the last timeslice which the Regions control.)
	pub region_end: Timeslice,
	/// The number of cores we want to sell, ideally. Selling this amount would result in no
	/// change to the price for the next sale.
	pub ideal_cores_sold: CoreIndex,
	/// Number of cores which are/have been offered for sale.
	pub cores_offered: CoreIndex,
	/// The index of the first core which is for sale. Core of Regions which are sold have
	/// incrementing indices from this.
	pub first_core: CoreIndex,
	/// The price at which cores have been sold out.
	///
	/// Will only be `None` if no core was offered for sale.
	pub sellout_price: Option<Balance>,
	/// Number of cores which have been sold; never more than cores_offered.
	pub cores_sold: CoreIndex,
}

/// Configuration of the coretime system.
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen,
)]
pub struct ConfigRecord<BlockNumber> {
	/// The number of Relay-chain blocks in advance which scheduling should be fixed and the
	/// `Coretime::assign` API used to inform the Relay-chain.
	pub advance_notice: BlockNumber,
	/// The length in blocks of the Interlude Period for forthcoming sales.
	pub interlude_length: BlockNumber,
	/// The length in blocks of the Leadin Period for forthcoming sales.
	pub leadin_length: BlockNumber,
	/// The length in timeslices of Regions which are up for sale in forthcoming sales.
	pub region_length: Timeslice,
	/// The proportion of cores available for sale which should be sold.
	///
	/// If more cores are sold than this, then further sales will no longer be considered in
	/// determining the sellout price. In other words the sellout price will be the last price
	/// paid, without going over this limit.
	pub ideal_bulk_proportion: Perbill,
	/// An artificial limit to the number of cores which are allowed to be sold. If `Some` then
	/// no more cores will be sold than this.
	pub limit_cores_offered: Option<CoreIndex>,
	/// The amount by which the renewal price increases each sale period.
	pub renewal_bump: Perbill,
	/// The duration by which rewards for contributions to the InstaPool must be collected.
	pub contribution_timeout: Timeslice,
}

impl<BlockNumber> ConfigRecord<BlockNumber>
where
	BlockNumber: sp_arithmetic::traits::Zero,
{
	/// Check the config for basic validity constraints.
	pub fn validate(&self) -> Result<(), ()> {
		if self.leadin_length.is_zero() {
			return Err(());
		}

		Ok(())
	}
}

/// General status of the system.
#[derive(Encode, Decode, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen)]
pub struct StatusRecord {
	/// The total number of cores which can be assigned (one plus the maximum index which can
	/// be used in `Coretime::assign`).
	pub core_count: CoreIndex,
	/// The current size of the Instantaneous Coretime Pool, measured in Core Mask Bits.
	pub private_pool_size: CoreMaskBitCount,
	/// The current amount of the Instantaneous Coretime Pool which is provided by the Polkadot
	/// System, rather than provided as a result of privately operated Coretime.
	pub system_pool_size: CoreMaskBitCount,
	/// The last (Relay-chain) timeslice which we committed to the Relay-chain.
	pub last_committed_timeslice: Timeslice,
	/// The timeslice of the last time we ticked.
	pub last_timeslice: Timeslice,
}

/// Performance of a past sale.
#[derive(Copy, Clone)]
pub struct SalePerformance<Balance> {
	/// The price at which the last core was sold.
	///
	/// Will be `None` if no cores have been offered.
	pub sellout_price: Option<Balance>,
	/// The minimum price that was achieved in this sale.
	pub end_price: Balance,
	/// The number of cores we want to sell, ideally.
	pub ideal_cores_sold: CoreIndex,
	/// Number of cores which are/have been offered for sale.
	pub cores_offered: CoreIndex,
	/// Number of cores which have been sold; never more than cores_offered.
	pub cores_sold: CoreIndex,
}

impl<Balance: Copy> SalePerformance<Balance> {
	/// Construct performance via data from a `SaleInfoRecord`.
	pub fn from_sale<BlockNumber>(record: &SaleInfoRecord<Balance, BlockNumber>) -> Self {
		Self {
			sellout_price: record.sellout_price,
			end_price: record.end_price,
			ideal_cores_sold: record.ideal_cores_sold,
			cores_offered: record.cores_offered,
			cores_sold: record.cores_sold,
		}
	}

	#[cfg(test)]
	fn new(sellout_price: Option<Balance>, end_price: Balance) -> Self {
		Self { sellout_price, end_price, ideal_cores_sold: 0, cores_offered: 0, cores_sold: 0 }
	}
}

/// Result of `AdaptPrice::adapt_price`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct AdaptedPrices<Balance> {
	/// New minimum price to use.
	pub end_price: Balance,
	/// Price the controller is optimizing for.
	///
	/// This is the price "expected" by the controller based on the previous sale. We assume that
	/// sales in this period will be around this price, assuming stable market conditions.
	///
	/// Think of it as the expected market price. This can be used for determining what to charge
	/// for renewals, that don't yet have any price information for example. E.g. for expired
	/// legacy leases.
	pub target_price: Balance,
}

/// Type for determining how to set price.
pub trait AdaptPrice<Balance> {
	/// Return adapted prices for next sale.
	///
	/// Based on the previous sale's performance.
	fn adapt_price(performance: SalePerformance<Balance>) -> AdaptedPrices<Balance>;
}

impl<Balance: Copy> AdaptPrice<Balance> for () {
	fn adapt_price(performance: SalePerformance<Balance>) -> AdaptedPrices<Balance> {
		let price = performance.sellout_price.unwrap_or(performance.end_price);
		AdaptedPrices { end_price: price, target_price: price }
	}
}

/// Simple implementation of `AdaptPrice` with two linear phases.
///
/// One steep one downwards to the target price, which is 1/10 of the maximum price and a more flat
/// one down to the minimum price, which is 1/100 of the maximum price.
pub struct CenterTargetPrice<Balance>(core::marker::PhantomData<Balance>);

impl<Balance: FixedPointOperand> AdaptPrice<Balance> for CenterTargetPrice<Balance> {
	fn adapt_price(performance: SalePerformance<Balance>) -> AdaptedPrices<Balance> {
		let Some(sellout_price) = performance.sellout_price else {
			return AdaptedPrices {
				end_price: performance.end_price,
				target_price: FixedU64::from(10).saturating_mul_int(performance.end_price),
			};
		};

		let price = FixedU64::from_rational(1, 10).saturating_mul_int(sellout_price);
		let price = if price == Balance::zero() {
			// We could not recover from a price equal 0 ever.
			sellout_price
		} else {
			price
		};

		AdaptedPrices { end_price: price, target_price: sellout_price }
	}
}

/// `AdaptPrice` like `CenterTargetPrice`, but with a minimum price.
///
/// This price adapter behaves exactly like `CenterTargetPrice`, except that it takes a minimum
/// price and makes sure that the returned `end_price` is never lower than that.
///
/// Target price will also get adjusted if necessary (it will never be less than the end_price).
pub struct MinimumPrice<Balance, MinPrice>(core::marker::PhantomData<(Balance, MinPrice)>);

impl<Balance: FixedPointOperand, MinPrice: sp_core::Get<Balance>> AdaptPrice<Balance>
	for MinimumPrice<Balance, MinPrice>
{
	fn adapt_price(performance: SalePerformance<Balance>) -> AdaptedPrices<Balance> {
		let mut proposal = CenterTargetPrice::<Balance>::adapt_price(performance);
		let min_price = MinPrice::get();
		if proposal.end_price < min_price {
			proposal.end_price = min_price;
		}
		if proposal.target_price < proposal.end_price {
			proposal.target_price = proposal.end_price;
		}
		proposal
	}
}

/// Trait for providing the reserved core count.
pub trait CoreCountProvider {
	/// Returns the number of reserved cores (reservations + leases).
	fn reserved_core_count() -> CoreIndex;
}

/// Errors specific to market operations.
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
	type BidId;
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
	) -> Vec<
		TickAction<Self::Balance, Self::BlockNumber, Self::AccountId, Self::BidId>,
	>;
}

#[cfg(test)]
mod tests {
	use super::*;
	use sp_core::ConstU64;

	#[test]
	fn region_id_converts_u128() {
		let r = RegionId {
			begin: 0x12345678u32,
			core: 0xabcdu16,
			mask: 0xdeadbeefcafef00d0123.into(),
		};
		let u = 0x12345678_abcd_deadbeefcafef00d0123u128;
		assert_eq!(RegionId::from(u), r);
		assert_eq!(u128::from(r), u);
	}

	#[test]
	fn core_mask_complete_works() {
		assert_eq!(CoreMask::complete(), CoreMask([0xff; 10]));
		assert!(CoreMask([0xff; 10]).is_complete());
		for i in 0..80 {
			assert!(!CoreMask([0xff; 10]).clear(i).is_complete());
		}
	}

	#[test]
	fn core_mask_void_works() {
		assert_eq!(CoreMask::void(), CoreMask([0; 10]));
		assert!(CoreMask([0; 10]).is_void());
		for i in 0..80 {
			assert!(!(CoreMask([0; 10]).set(i).is_void()));
		}
	}

	#[test]
	fn adapt_price_no_panic() {
		for sellout in 0..11 {
			for price in 0..10 {
				let sellout_price = if sellout == 11 { None } else { Some(sellout) };
				CenterTargetPrice::adapt_price(SalePerformance::new(sellout_price, price));
			}
		}
	}

	#[test]
	fn no_op_sale_is_good() {
		let prices = CenterTargetPrice::adapt_price(SalePerformance::new(None, 1));
		assert_eq!(prices.target_price, 10);
		assert_eq!(prices.end_price, 1);
	}

	#[test]
	fn price_stays_stable_on_optimal_sale() {
		let mut performance = SalePerformance::new(Some(1000), 100);
		for _ in 0..10 {
			let prices = CenterTargetPrice::adapt_price(performance);
			performance.sellout_price = Some(1000);
			performance.end_price = prices.end_price;

			assert!(prices.end_price <= 101);
			assert!(prices.end_price >= 99);
			assert!(prices.target_price <= 1001);
			assert!(prices.target_price >= 999);
		}
	}

	#[test]
	fn price_adjusts_correctly_upwards() {
		let performance = SalePerformance::new(Some(10_000), 100);
		let prices = CenterTargetPrice::adapt_price(performance);
		assert_eq!(prices.target_price, 10_000);
		assert_eq!(prices.end_price, 1000);
	}

	#[test]
	fn price_adjusts_correctly_downwards() {
		let performance = SalePerformance::new(Some(100), 100);
		let prices = CenterTargetPrice::adapt_price(performance);
		assert_eq!(prices.target_price, 100);
		assert_eq!(prices.end_price, 10);
	}

	#[test]
	fn price_never_goes_to_zero_and_recovers() {
		let sellout_price = 1;
		let mut performance = SalePerformance::new(Some(sellout_price), 1);
		for _ in 0..11 {
			let prices = CenterTargetPrice::adapt_price(performance);
			performance.sellout_price = Some(sellout_price);
			performance.end_price = prices.end_price;

			assert!(prices.end_price <= sellout_price);
			assert!(prices.end_price > 0);
		}
	}

	#[test]
	fn minimum_price_works() {
		let performance = SalePerformance::new(Some(10), 10);
		let prices = MinimumPrice::<u64, ConstU64<10>>::adapt_price(performance);
		assert_eq!(prices.end_price, 10);
		assert_eq!(prices.target_price, 10);
	}

	#[test]
	fn minimum_price_does_not_affect_valid_target_price() {
		let performance = SalePerformance::new(Some(12), 10);
		let prices = MinimumPrice::<u64, ConstU64<10>>::adapt_price(performance);
		assert_eq!(prices.end_price, 10);
		assert_eq!(prices.target_price, 12);
	}

	#[test]
	fn no_minimum_price_works_as_center_target_price() {
		let performances = [
			(Some(100), 10),
			(None, 20),
			(Some(1000), 10),
			(Some(10), 10),
			(Some(1), 1),
			(Some(0), 10),
		];
		for (sellout, end) in performances {
			let performance = SalePerformance::new(sellout, end);
			let prices_minimum = MinimumPrice::<u64, ConstU64<0>>::adapt_price(performance);
			let prices = CenterTargetPrice::adapt_price(performance);
			assert_eq!(prices, prices_minimum);
		}
	}
}
