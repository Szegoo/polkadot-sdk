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

mod adapt_price;
mod core_mask;
mod market;

pub use adapt_price::*;
pub use core_mask::*;
pub use market::*;

use codec::{Decode, DecodeWithMemTracking, Encode, MaxEncodedLen};
use scale_info::TypeInfo;
use sp_arithmetic::Perbill;

/// Index of a Polkadot Core.
pub type CoreIndex = u16;

/// A Task Id. In general this is called a ParachainId.
pub type TaskId = u32;

/// Relay-chain block number with a fixed divisor of TimeslicePeriod.
pub type Timeslice = u32;

/// Fraction expressed as a nominator with an assumed denominator of 57,600.
pub type PartsOf57600 = u16;

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
	Default,
	TypeInfo,
	MaxEncodedLen,
)]
pub enum SalePhase {
	/// Market period: descending Dutch auction, bids accepted.
	#[default]
	Market,
	/// Renewal period: existing tenants can exercise renewal rights.
	Renewal,
	/// Settlement period: secondary market trading only, no primary sales.
	Settlement,
}

/// The status of a Bulk Coretime Sale.
#[derive(Encode, Decode, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen)]
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
	pub penalty: Perbill,
	/// The duration by which rewards for contributions to the InstaPool must be collected.
	pub contribution_timeout: Timeslice,
}

impl<BlockNumber> ConfigRecord<BlockNumber>
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

#[cfg(test)]
mod tests {
	use super::*;

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
}
