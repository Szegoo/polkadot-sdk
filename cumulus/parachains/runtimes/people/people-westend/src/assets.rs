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

use super::*;

use frame_support::{parameter_types, traits::fungibles::Balanced};
use frame_system::{EnsureNever, EnsureRoot};
use sp_runtime::traits::AccountIdConversion;
use xcm::latest::{Asset, AssetId, Junction::*, Location};

parameter_types! {
	pub const AssetDeposit: Balance = UNITS;
	pub const AssetAccountDeposit: Balance = deposit(1, 16);
	pub const ApprovalDeposit: Balance = EXISTENTIAL_DEPOSIT;
	pub const AssetsStringLimit: u32 = 50;
	pub const MetadataDepositBase: Balance = deposit(1, 68);
	pub const MetadataDepositPerByte: Balance = deposit(0, 1);
}

impl pallet_assets::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type Balance = Balance;
	type AssetId = Location;
	type AssetIdParameter = Location;
	type Currency = Balances;
	// Assets can only be force created by root.
	type CreateOrigin = EnsureNever<AccountId>;
	type ForceOrigin = EnsureRoot<AccountId>;
	type AssetDeposit = AssetDeposit;
	type MetadataDepositBase = MetadataDepositBase;
	type MetadataDepositPerByte = MetadataDepositPerByte;
	type ApprovalDeposit = ApprovalDeposit;
	type StringLimit = AssetsStringLimit;
	type Holder = AssetsHolder;
	type Freezer = ();
	type Extra = ();
	type CallbackHandle = ();
	type WeightInfo = weights::pallet_assets_foreign::WeightInfo<Runtime>;
	type AssetAccountDeposit = AssetAccountDeposit;
	type ReserveData = ();
	type RemoveItemsLimit = ConstU32<1000>;
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = xcm_config::XcmBenchmarkHelper;
}

/// Handles crediting asset transaction fees to the DAP satellite account,
/// consistent with how native WND fees are handled via `DealWithFeesSatellite`.
pub struct CreditToDapSatellite;
impl pallet_asset_tx_payment::HandleCredit<AccountId, Assets> for CreditToDapSatellite {
	fn handle_credit(credit: frame_support::traits::fungibles::Credit<AccountId, Assets>) {
		let dap_account: AccountId = DapSatellitePalletId::get().into_account_truncating();
		let _ = Assets::resolve(&dap_account, credit);
	}
}

type OnChargeStableTransaction =
	pallet_asset_tx_payment::FungiblesAdapter<AssetRate, CreditToDapSatellite>;

#[cfg(feature = "runtime-benchmarks")]
pub struct AssetTxPaymentBenchmarkHelper;
#[cfg(feature = "runtime-benchmarks")]
impl pallet_asset_tx_payment::BenchmarkHelperTrait<AccountId, Location, Location>
	for AssetTxPaymentBenchmarkHelper
{
	fn create_asset_id_parameter(id: u32) -> (Location, Location) {
		assert_eq!(id, 1);
		let l = Location::new(
			1,
			[
				xcm::latest::Junction::Parachain(1000),
				xcm::latest::Junction::PalletInstance(50),
				xcm::latest::Junction::GeneralIndex(1337),
			],
		);
		(l.clone(), l)
	}

	fn setup_balances_and_pool(asset_id: Location, account: AccountId) {
		use alloc::boxed::Box;
		use frame_support::traits::{
			fungible::Mutate as _,
			fungibles::{Inspect as _, Mutate as _},
		};

		AssetRate::create(RuntimeOrigin::root(), Box::new(asset_id.clone()), 1.into()).unwrap();
		if !Assets::asset_exists(asset_id.clone()) {
			Assets::force_create(
				RuntimeOrigin::root(),
				asset_id.clone(),
				account.clone().into(),
				true,
				1,
			)
			.unwrap();
		}
		Assets::mint_into(asset_id, &account, 10_000 * UNITS).unwrap();
		Balances::mint_into(&account, 10_000 * UNITS).unwrap();
	}
}

impl pallet_asset_tx_payment::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type Fungibles = Assets;
	type OnChargeAssetTransaction = OnChargeStableTransaction;
	type WeightInfo = weights::pallet_asset_tx_payment::WeightInfo<Runtime>;
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = AssetTxPaymentBenchmarkHelper;
}

impl pallet_asset_rate::Config for Runtime {
	type WeightInfo = weights::pallet_asset_rate::WeightInfo<Runtime>;
	type RuntimeEvent = RuntimeEvent;
	type CreateOrigin = EnsureRoot<AccountId>;
	type RemoveOrigin = EnsureRoot<AccountId>;
	type UpdateOrigin = EnsureRoot<AccountId>;
	type Currency = Balances;
	type AssetKind = <Runtime as pallet_assets::Config>::AssetId;
	#[cfg(feature = "runtime-benchmarks")]
	type BenchmarkHelper = AssetRateBenchmarkHelper;
}

#[cfg(feature = "runtime-benchmarks")]
pub struct AssetRateBenchmarkHelper;

#[cfg(feature = "runtime-benchmarks")]
impl pallet_asset_rate::AssetKindFactory<Location> for AssetRateBenchmarkHelper {
	fn create_asset_kind(seed: u32) -> Location {
		Location::new(
			1,
			[
				xcm::latest::Junction::Parachain(1000),
				xcm::latest::Junction::GeneralIndex(seed as u128),
			],
		)
	}
}

impl pallet_assets_holder::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type RuntimeHoldReason = RuntimeHoldReason;
}

/// Module that holds everything related to the pUSD asset.
pub mod pusd {
	use super::*;

	/// The Asset Hub parachain ID.
	pub const ASSET_HUB_PARA_ID: u32 = 1000;

	/// The pUSD asset ID on Asset Hub.
	pub const PUSD_ASSET_ID: u128 = 50000342;

	/// A unit of pUSD consists of 10^6 plancks.
	pub const PUSD_UNITS: u128 = 1_000_000;

	parameter_types! {
		pub AssetHubLocation: Location = Location::new(1, [Parachain(ASSET_HUB_PARA_ID)]);
		pub PUsdLocation: Location = Location::new(
			1,
			[Parachain(ASSET_HUB_PARA_ID), PalletInstance(50), GeneralIndex(PUSD_ASSET_ID)]
		);
		pub PUsdId: AssetId = AssetId(PUsdLocation::get());
		pub PUsd: Asset = (PUsdId::get(), 10 * PUSD_UNITS).into();
	}
}
