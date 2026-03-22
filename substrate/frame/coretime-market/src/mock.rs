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

use crate::*;
use frame_support::derive_impl;
use sp_core::ConstU32;
use sp_coretime::{CoreCountProvider, CoreIndex, RenewalRightsProvider};
use sp_runtime::BuildStorage;

type Block = frame_system::mocking::MockBlock<Test>;

frame_support::construct_runtime!(
	pub enum Test
	{
		System: frame_system,
		CoretimeMarket: crate,
	}
);

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
}

pub struct TestCoreCountProvider;
impl CoreCountProvider for TestCoreCountProvider {
	fn reserved_core_count() -> CoreIndex {
		0
	}
}

/// Mock renewal rights provider. Stores renewal rights in a thread-local.
pub struct TestRenewalRights;

thread_local! {
	static RENEWAL_RIGHTS: core::cell::RefCell<alloc::collections::BTreeMap<(u64, Timeslice), u32>> =
		core::cell::RefCell::new(Default::default());
}

impl TestRenewalRights {
	pub fn set(who: u64, when: Timeslice, count: u32) {
		RENEWAL_RIGHTS.with(|r| {
			r.borrow_mut().insert((who, when), count);
		});
	}
}

impl RenewalRightsProvider<u64> for TestRenewalRights {
	fn renewal_rights_count(who: &u64, when: Timeslice) -> u32 {
		RENEWAL_RIGHTS.with(|r| r.borrow().get(&(*who, when)).copied().unwrap_or(0))
	}
}

impl crate::pallet::Config for Test {
	type Balance = u64;
	type RelayBlockNumber = u64;
	type WeightInfo = ();
	type CoreCountProvider = TestCoreCountProvider;
	type RenewalRights = TestRenewalRights;
	type TimeslicePeriod = sp_core::ConstU64<2>;
	type MaxBids = ConstU32<100>;
}

pub fn new_config() -> ConfigRecord<u64, u64> {
	ConfigRecord {
		advance_notice: 2,
		market_period: 20,
		renewal_period: 10,
		ideal_bulk_proportion: sp_arithmetic::Perbill::from_percent(100),
		limit_cores_offered: None,
		region_length: 3,
		penalty: sp_arithmetic::Perbill::from_percent(30),
		contribution_timeout: 5,
		price_multiplier: 2,
		min_opening_price: 10,
		target_consumption_rate: sp_arithmetic::Perbill::from_percent(90),
		sensitivity_millis: 2500, // K = 2.5
		min_reserve_price: 1,
		min_increment: 100,
	}
}

pub fn new_test_ext() -> sp_io::TestExternalities {
	let c = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	sp_io::TestExternalities::from(c)
}

pub struct TestExt(ConfigRecord<u64, u64>);
#[allow(dead_code)]
impl TestExt {
	pub fn new() -> Self {
		Self(new_config())
	}

	pub fn new_with_config(config: ConfigRecord<u64, u64>) -> Self {
		Self(config)
	}

	pub fn execute_with<R>(self, f: impl Fn() -> R) -> R {
		new_test_ext().execute_with(|| {
			<CoretimeMarket as MarketState>::set_configuration(self.0);
			f()
		})
	}
}
