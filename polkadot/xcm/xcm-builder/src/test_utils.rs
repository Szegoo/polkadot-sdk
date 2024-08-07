// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

// Shared test utilities and implementations for the XCM Builder.

use alloc::vec::Vec;
use frame_support::{
	parameter_types,
	traits::{Contains, CrateVersion, PalletInfoData, PalletsInfoAccess},
};
pub use xcm::latest::{prelude::*, Weight};
use xcm_executor::traits::{ClaimAssets, DropAssets, VersionChangeNotifier};
pub use xcm_executor::{
	traits::{
		AssetExchange, AssetLock, ConvertOrigin, Enact, LockError, OnResponse, TransactAsset,
	},
	AssetsInHolding, Config,
};

parameter_types! {
	pub static SubscriptionRequests: Vec<(Location, Option<(QueryId, Weight)>)> = vec![];
	pub static MaxAssetsIntoHolding: u32 = 4;
}

pub struct TestSubscriptionService;

impl VersionChangeNotifier for TestSubscriptionService {
	fn start(
		location: &Location,
		query_id: QueryId,
		max_weight: Weight,
		_context: &XcmContext,
	) -> XcmResult {
		let mut r = SubscriptionRequests::get();
		r.push((location.clone(), Some((query_id, max_weight))));
		SubscriptionRequests::set(r);
		Ok(())
	}
	fn stop(location: &Location, _context: &XcmContext) -> XcmResult {
		let mut r = SubscriptionRequests::get();
		r.retain(|(l, _q)| l != location);
		r.push((location.clone(), None));
		SubscriptionRequests::set(r);
		Ok(())
	}
	fn is_subscribed(location: &Location) -> bool {
		let r = SubscriptionRequests::get();
		r.iter().any(|(l, q)| l == location && q.is_some())
	}
}

parameter_types! {
	pub static TrappedAssets: Vec<(Location, Assets)> = vec![];
}

pub struct TestAssetTrap;

impl DropAssets for TestAssetTrap {
	fn drop_assets(origin: &Location, assets: AssetsInHolding, _context: &XcmContext) -> Weight {
		let mut t: Vec<(Location, Assets)> = TrappedAssets::get();
		t.push((origin.clone(), assets.into()));
		TrappedAssets::set(t);
		Weight::from_parts(5, 5)
	}
}

impl ClaimAssets for TestAssetTrap {
	fn claim_assets(
		origin: &Location,
		ticket: &Location,
		what: &Assets,
		_context: &XcmContext,
	) -> bool {
		let mut t: Vec<(Location, Assets)> = TrappedAssets::get();
		if let (0, [GeneralIndex(i)]) = ticket.unpack() {
			if let Some((l, a)) = t.get(*i as usize) {
				if l == origin && a == what {
					t.swap_remove(*i as usize);
					TrappedAssets::set(t);
					return true
				}
			}
		}
		false
	}
}

pub struct TestAssetExchanger;

impl AssetExchange for TestAssetExchanger {
	fn exchange_asset(
		_origin: Option<&Location>,
		_give: AssetsInHolding,
		want: &Assets,
		_maximal: bool,
	) -> Result<AssetsInHolding, AssetsInHolding> {
		Ok(want.clone().into())
	}
}

pub struct TestPalletsInfo;
impl PalletsInfoAccess for TestPalletsInfo {
	fn count() -> usize {
		2
	}
	fn infos() -> Vec<PalletInfoData> {
		vec![
			PalletInfoData {
				index: 0,
				name: "System",
				module_name: "pallet_system",
				crate_version: CrateVersion { major: 1, minor: 10, patch: 1 },
			},
			PalletInfoData {
				index: 1,
				name: "Balances",
				module_name: "pallet_balances",
				crate_version: CrateVersion { major: 1, minor: 42, patch: 69 },
			},
		]
	}
}

pub struct TestUniversalAliases;
impl Contains<(Location, Junction)> for TestUniversalAliases {
	fn contains(aliases: &(Location, Junction)) -> bool {
		&aliases.0 == &Here.into_location() && &aliases.1 == &GlobalConsensus(ByGenesis([0; 32]))
	}
}

parameter_types! {
	pub static LockedAssets: Vec<(Location, Asset)> = vec![];
}

pub struct TestLockTicket(Location, Asset);
impl Enact for TestLockTicket {
	fn enact(self) -> Result<(), LockError> {
		let mut locked_assets = LockedAssets::get();
		locked_assets.push((self.0, self.1));
		LockedAssets::set(locked_assets);
		Ok(())
	}
}
pub struct TestUnlockTicket(Location, Asset);
impl Enact for TestUnlockTicket {
	fn enact(self) -> Result<(), LockError> {
		let mut locked_assets = LockedAssets::get();
		if let Some((idx, _)) = locked_assets
			.iter()
			.enumerate()
			.find(|(_, (origin, asset))| origin == &self.0 && asset == &self.1)
		{
			locked_assets.remove(idx);
		}
		LockedAssets::set(locked_assets);
		Ok(())
	}
}
pub struct TestReduceTicket;
impl Enact for TestReduceTicket {
	fn enact(self) -> Result<(), LockError> {
		Ok(())
	}
}

pub struct TestAssetLocker;
impl AssetLock for TestAssetLocker {
	type LockTicket = TestLockTicket;
	type UnlockTicket = TestUnlockTicket;
	type ReduceTicket = TestReduceTicket;

	fn prepare_lock(
		unlocker: Location,
		asset: Asset,
		_owner: Location,
	) -> Result<TestLockTicket, LockError> {
		Ok(TestLockTicket(unlocker, asset))
	}

	fn prepare_unlock(
		unlocker: Location,
		asset: Asset,
		_owner: Location,
	) -> Result<TestUnlockTicket, LockError> {
		Ok(TestUnlockTicket(unlocker, asset))
	}

	fn note_unlockable(
		_locker: Location,
		_asset: Asset,
		_owner: Location,
	) -> Result<(), LockError> {
		Ok(())
	}

	fn prepare_reduce_unlockable(
		_locker: Location,
		_asset: Asset,
		_owner: Location,
	) -> Result<TestReduceTicket, LockError> {
		Ok(TestReduceTicket)
	}
}
