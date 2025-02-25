// SPDX-License-Identifier: Apache-2.0
// This file is part of Frontier.
//
// Copyright (c) 2021 Parity Technologies (UK) Ltd.
//
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

// Ensure we're `no_std` when compiling for Wasm.
#![cfg_attr(not(feature = "std"), no_std)]

use codec::{Encode, Decode};
use sp_std::{result, cmp::{min, max}};
use sp_runtime::RuntimeDebug;
use sp_core::U256;
use sp_inherents::{InherentIdentifier, InherentData, IsFatalError};
use frame_support::{traits::Get};

use frame_system::ensure_none;
pub use pallet::*;

/// Implementation of Mixer pallet
#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The overarching event type.
		type Event: IsType<<Self as frame_system::Config>::Event> + From<Event<Self>>;
		/// Bound divisor for min gas price.
		type MinGasPriceBoundDivisor: Get<U256>;
	}

	#[pallet::storage]
	#[pallet::getter(fn target_min_gas_price)]
	pub type TargetMinGasPrice<T: Config> = StorageValue<_, Option<U256>, ValueQuery>;

	#[pallet::storage]
	#[pallet::getter(fn min_gas_price)]
	pub type MinGasPrice<T: Config> = StorageValue<_, U256, ValueQuery>;

	#[pallet::genesis_config]
	pub struct GenesisConfig {
		pub gas_price: U256,
	}

	#[cfg(feature = "std")]
	impl Default for GenesisConfig {
		fn default() -> Self {
			Self {
				gas_price: U256::from(1),
			}
		}
	}

	#[pallet::genesis_build]
	impl<T: Config> GenesisBuild<T> for GenesisConfig {
		fn build(&self) {
			MinGasPrice::<T>::set(U256::from(1));
		}
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		TargetMinGasPriceSet(U256)
	}

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	pub struct Pallet<T>(PhantomData<T>);

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_finalize(_n: T::BlockNumber) {
			if let Some(target) = TargetMinGasPrice::<T>::take() {
				let bound = MinGasPrice::<T>::get() / T::MinGasPriceBoundDivisor::get() + U256::one();

				let upper_limit = MinGasPrice::<T>::get().saturating_add(bound);
				let lower_limit = MinGasPrice::<T>::get().saturating_sub(bound);

				MinGasPrice::<T>::set(min(upper_limit, max(lower_limit, target)));
			}

			TargetMinGasPrice::<T>::kill();
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		#[pallet::weight(0)]
		fn note_min_gas_price_target(
			origin: OriginFor<T>,
			target: U256,
		) -> DispatchResultWithPostInfo {
			ensure_none(origin)?;
			assert!(TargetMinGasPrice::<T>::get().is_none(), "TargetMinGasPrice must be updated only once in the block");

			TargetMinGasPrice::<T>::set(Some(target));
			Self::deposit_event(Event::TargetMinGasPriceSet(target));
			Ok(().into())
		}
	}

	#[pallet::inherent]
	impl<T: Config> ProvideInherent for Pallet<T> {
		type Call = Call<T>;
		type Error = InherentError;
		const INHERENT_IDENTIFIER: InherentIdentifier = INHERENT_IDENTIFIER;

		fn create_inherent(data: &InherentData) -> Option<Self::Call> {
			let target = data.get_data::<InherentType>(&INHERENT_IDENTIFIER).ok()??;

			Some(Call::note_min_gas_price_target(target))
		}

		fn check_inherent(_call: &Self::Call, _data: &InherentData) -> result::Result<(), Self::Error> {
			Ok(())
		}

		fn is_inherent(call: &Self::Call) -> bool {
			matches!(call, Call::note_min_gas_price_target(_))
		}
	}
}

impl<T: Config> pallet_evm::FeeCalculator for Pallet<T> {
	fn min_gas_price() -> U256 {
		MinGasPrice::<T>::get()
	}
}

#[derive(Encode, Decode, RuntimeDebug)]
pub enum InherentError { }

impl IsFatalError for InherentError {
	fn is_fatal_error(&self) -> bool {
		match *self { }
	}
}

impl InherentError {
	/// Try to create an instance ouf of the given identifier and data.
	#[cfg(feature = "std")]
	pub fn try_from(id: &InherentIdentifier, data: &[u8]) -> Option<Self> {
		if id == &INHERENT_IDENTIFIER {
			<InherentError as codec::Decode>::decode(&mut &data[..]).ok()
		} else {
			None
		}
	}
}

pub const INHERENT_IDENTIFIER: InherentIdentifier = *b"dynfee0_";
pub type InherentType = U256;


#[cfg(feature = "std")]
pub struct InherentDataProvider(pub InherentType);

#[cfg(feature = "std")]
#[async_trait::async_trait]
impl sp_inherents::InherentDataProvider for InherentDataProvider {
	fn provide_inherent_data(
		&self,
		inherent_data: &mut InherentData
	) -> Result<(), sp_inherents::Error> {
		inherent_data.put_data(INHERENT_IDENTIFIER, &self.0)
	}

	async fn try_handle_error(
		&self,
		identifier: &InherentIdentifier,
		error: &[u8],
	) -> Option<Result<(), sp_inherents::Error>> {
		if *identifier != INHERENT_IDENTIFIER {
			return None
		}

		match InherentError::try_from(&INHERENT_IDENTIFIER, error) {
			Some(err) => {
				let error_string: &str = &format!("{:?}", err).to_owned();
				let error = sp_inherents::Error::Application(Box::from(error_string));
				Some(Err(error))
			},
			None => None,
		}
	}
}
