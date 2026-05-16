// This file is part of Substrate.

// Copyright (C) Amforc AG.
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

//! Benchmarking setup for pallet-psm

use super::*;
use crate::{
	pallet::{BalanceOf, PsmInfo},
	Pallet as Psm,
};
use frame_benchmarking::v2::*;
use frame_support::traits::{
	fungibles::{Inspect as FungiblesInspect, Mutate as FungiblesMutate},
	Get,
};
use frame_system::RawOrigin;
use sp_runtime::{Permill, Saturating};

/// Offset for benchmark external asset IDs, chosen to avoid collision with the
/// internal asset.
const ASSET_ID_OFFSET: u32 = 100;
/// Asset id used as the benchmark PSM's internal asset.
const INTERNAL_ID_OFFSET: u32 = 99;

/// Set up the benchmark PSM (internal asset + `Psms` entry) and `n` approved
/// externals. Returns `(internal_asset, target_external)`.
///
/// The first `n - 1` externals are filler — they populate per-PSM storage so the
/// iterators in `total_psm_debt()` and `max_asset_debt()` touch `n` entries during
/// `mint()`. The `target_external` (last one) carries dominant weight so it can
/// absorb the full mint amount.
fn setup_assets<T: Config>(n: u32) -> (T::AssetId, T::AssetId) {
	let admin: T::AccountId = whitelisted_caller();
	let _ = frame_system::Pallet::<T>::inc_providers(&admin);

	let internal_asset = T::BenchmarkHelper::get_asset_id(INTERNAL_ID_OFFSET);
	let internal_decimals = 6u8;
	if !T::Fungibles::asset_exists(internal_asset.clone()) {
		T::BenchmarkHelper::create_asset(internal_asset.clone(), &admin, internal_decimals);
	}

	if !crate::Psms::<T>::contains_key(&internal_asset) {
		Psm::<T>::ensure_account_exists(&Psm::<T>::psm_account(&internal_asset));
		Psm::<T>::ensure_account_exists(&admin);
		crate::Psms::<T>::insert(
			&internal_asset,
			PsmInfo::<T> {
				fee_destination: admin.clone(),
				max_debt: BalanceOf::<T>::from(u32::MAX).saturating_mul(1_000_000u32.into()),
				internal_decimals,
				external_count: 0,
			},
		);
	}

	let target_id: T::AssetId = T::BenchmarkHelper::get_asset_id(ASSET_ID_OFFSET + n - 1);
	if !T::Fungibles::asset_exists(target_id.clone()) {
		T::BenchmarkHelper::create_asset(target_id.clone(), &admin, internal_decimals);
	}

	// Filler externals.
	for i in 0..n {
		let id: T::AssetId = T::BenchmarkHelper::get_asset_id(ASSET_ID_OFFSET + i);
		crate::ExternalAssets::<T>::insert(&internal_asset, &id, CircuitBreakerLevel::AllEnabled);
		let weight = if id == target_id {
			Permill::from_percent(100)
		} else {
			Permill::from_percent(1)
		};
		crate::AssetCeilingWeight::<T>::insert(&internal_asset, &id, weight);
		if id != target_id {
			crate::PsmDebt::<T>::insert(&internal_asset, &id, BalanceOf::<T>::from(1u32));
		}
	}
	crate::ExternalDecimals::<T>::insert(&internal_asset, &target_id, internal_decimals);
	crate::Psms::<T>::mutate(&internal_asset, |maybe| {
		if let Some(info) = maybe.as_mut() {
			info.external_count = n;
		}
	});

	(internal_asset, target_id)
}

#[benchmarks]
mod benchmarks {
	use super::*;

	/// Linear in `n`. The number of registered externals on a PSM, because
	/// `total_psm_debt()` iterates `PsmDebt` and `max_asset_debt()` iterates
	/// `AssetCeilingWeight`.
	#[benchmark]
	fn mint(n: Linear<1, { T::MaxExternalAssetsPerPsm::get() }>) -> Result<(), BenchmarkError> {
		let caller: T::AccountId = whitelisted_caller();
		let (internal_asset, asset_id) = setup_assets::<T>(n);
		let mint_amount = T::MinSwapAmount::get().saturating_mul(10u32.into());

		T::Fungibles::mint_into(asset_id.clone(), &caller, mint_amount.saturating_mul(2u32.into()))
			.map_err(|_| BenchmarkError::Stop("Failed to fund caller"))?;

		let psm_account = Psm::<T>::psm_account(&internal_asset);
		let reserve_before = T::Fungibles::balance(asset_id.clone(), &psm_account);

		#[extrinsic_call]
		_(RawOrigin::Signed(caller.clone()), internal_asset.clone(), asset_id.clone(), mint_amount);

		assert!(T::Fungibles::balance(asset_id, &psm_account) > reserve_before);
		Ok(())
	}

	#[benchmark]
	fn redeem() -> Result<(), BenchmarkError> {
		let caller: T::AccountId = whitelisted_caller();
		let (internal_asset, asset_id) = setup_assets::<T>(1);
		let setup_amount = T::MinSwapAmount::get().saturating_mul(10u32.into());
		let redeem_amount = T::MinSwapAmount::get();

		T::Fungibles::mint_into(
			asset_id.clone(),
			&caller,
			setup_amount.saturating_mul(2u32.into()),
		)
		.map_err(|_| BenchmarkError::Stop("Failed to fund caller"))?;
		Psm::<T>::mint(
			RawOrigin::Signed(caller.clone()).into(),
			internal_asset.clone(),
			asset_id.clone(),
			setup_amount,
		)
		.map_err(|_| BenchmarkError::Stop("Failed to setup reserve via mint"))?;

		let psm_account = Psm::<T>::psm_account(&internal_asset);
		let reserve_before = T::Fungibles::balance(asset_id.clone(), &psm_account);

		#[extrinsic_call]
		_(RawOrigin::Signed(caller.clone()), internal_asset, asset_id.clone(), redeem_amount);

		assert!(T::Fungibles::balance(asset_id, &psm_account) < reserve_before);
		Ok(())
	}

	#[benchmark]
	fn set_minting_fee() -> Result<(), BenchmarkError> {
		let (internal_asset, asset_id) = setup_assets::<T>(1);
		let new_fee = Permill::from_percent(2);

		#[extrinsic_call]
		_(RawOrigin::Root, internal_asset.clone(), asset_id.clone(), new_fee);

		assert_eq!(crate::MintingFee::<T>::get(&internal_asset, &asset_id), new_fee);
		Ok(())
	}

	#[benchmark]
	fn set_redemption_fee() -> Result<(), BenchmarkError> {
		let (internal_asset, asset_id) = setup_assets::<T>(1);
		let new_fee = Permill::from_percent(2);

		#[extrinsic_call]
		_(RawOrigin::Root, internal_asset.clone(), asset_id.clone(), new_fee);

		assert_eq!(crate::RedemptionFee::<T>::get(&internal_asset, &asset_id), new_fee);
		Ok(())
	}

	#[benchmark]
	fn set_max_debt() -> Result<(), BenchmarkError> {
		let (internal_asset, _) = setup_assets::<T>(1);
		let new_value = BalanceOf::<T>::from(123u32);

		#[extrinsic_call]
		_(RawOrigin::Root, internal_asset.clone(), new_value);

		assert_eq!(crate::Psms::<T>::get(&internal_asset).unwrap().max_debt, new_value);
		Ok(())
	}

	#[benchmark]
	fn set_asset_status() -> Result<(), BenchmarkError> {
		let (internal_asset, asset_id) = setup_assets::<T>(1);
		let new_status = CircuitBreakerLevel::MintingDisabled;

		#[extrinsic_call]
		_(RawOrigin::Root, internal_asset.clone(), asset_id.clone(), new_status);

		assert_eq!(crate::ExternalAssets::<T>::get(&internal_asset, &asset_id), Some(new_status));
		Ok(())
	}

	#[benchmark]
	fn set_asset_ceiling_weight() -> Result<(), BenchmarkError> {
		let (internal_asset, asset_id) = setup_assets::<T>(1);
		let new_weight = Permill::from_percent(50);

		#[extrinsic_call]
		_(RawOrigin::Root, internal_asset.clone(), asset_id.clone(), new_weight);

		assert_eq!(crate::AssetCeilingWeight::<T>::get(&internal_asset, &asset_id), new_weight);
		Ok(())
	}

	#[benchmark]
	fn add_external_asset() -> Result<(), BenchmarkError> {
		let (internal_asset, _) = setup_assets::<T>(0);
		let caller: T::AccountId = whitelisted_caller();
		let new_asset_id: T::AssetId = T::BenchmarkHelper::get_asset_id(ASSET_ID_OFFSET);
		T::BenchmarkHelper::create_asset(new_asset_id.clone(), &caller, 6);

		#[extrinsic_call]
		_(RawOrigin::Root, internal_asset.clone(), new_asset_id.clone());

		assert!(crate::ExternalAssets::<T>::contains_key(&internal_asset, &new_asset_id));
		Ok(())
	}

	#[benchmark]
	fn remove_external_asset() -> Result<(), BenchmarkError> {
		let (internal_asset, asset_id) = setup_assets::<T>(1);
		crate::PsmDebt::<T>::remove(&internal_asset, &asset_id);

		#[extrinsic_call]
		_(RawOrigin::Root, internal_asset.clone(), asset_id.clone());

		assert!(!crate::ExternalAssets::<T>::contains_key(&internal_asset, &asset_id));
		Ok(())
	}

	impl_benchmark_test_suite!(Psm, crate::mock::new_test_ext(), crate::mock::Test);
}
