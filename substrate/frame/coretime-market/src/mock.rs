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
use sp_core::ConstU64;
use sp_coretime::{CenterTargetPrice, CoreCountProvider, CoreIndex};
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

impl crate::pallet::Config for Test {
	type Balance = u64;
	type RelayBlockNumber = u64;
	type WeightInfo = ();
	type PriceAdapter = CenterTargetPrice<u64>;
	type CoreCountProvider = TestCoreCountProvider;
	type TimeslicePeriod = ConstU64<2>;
}

pub fn new_config() -> ConfigRecord<u64> {
	ConfigRecord {
		advance_notice: 2,
		interlude_length: 1,
		leadin_length: 1,
		ideal_bulk_proportion: Default::default(),
		limit_cores_offered: None,
		region_length: 3,
		renewal_bump: sp_arithmetic::Perbill::from_percent(10),
		contribution_timeout: 5,
	}
}

pub fn new_test_ext() -> sp_io::TestExternalities {
	let c = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	sp_io::TestExternalities::from(c)
}

pub struct TestExt(ConfigRecord<u64>);
#[allow(dead_code)]
impl TestExt {
	pub fn new() -> Self {
		Self(new_config())
	}

	pub fn new_with_config(config: ConfigRecord<u64>) -> Self {
		Self(config)
	}

	pub fn execute_with<R>(self, f: impl Fn() -> R) -> R {
		new_test_ext().execute_with(|| {
			<CoretimeMarket as MarketState>::set_configuration(self.0);
			f()
		})
	}
}
