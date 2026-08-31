// Copyright 2022 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Defines the structures needed for saving/restoring entropy devices.

use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};

use crate::devices::virtio::device::VirtioDeviceType;
use crate::devices::virtio::persist::{PersistError as VirtioStateError, VirtioDeviceState};
use crate::devices::virtio::queue::FIRECRACKER_MAX_QUEUE_SIZE;
use crate::devices::virtio::rng::{Entropy, EntropyError, RNG_NUM_QUEUES};
use crate::rate_limiter::RateLimiter;
use crate::rate_limiter::persist::RateLimiterState;
use crate::snapshot::Persist;
use crate::vstate::memory::GuestMemoryMmap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntropyState {
    pub virtio_state: VirtioDeviceState,
    rate_limiter_state: RateLimiterState,
    /// The configured seed (None = default seed). Kept for config readback.
    seed: Option<u64>,
    /// Full ChaCha stream state: a restored device resumes the entropy stream
    /// at the exact position it was snapshotted at. This is what makes entropy
    /// deterministic across snapshot/branch boundaries.
    rng_state: ChaCha8Rng,
}

#[derive(Debug)]
pub struct EntropyConstructorArgs {
    pub mem: GuestMemoryMmap,
}

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum EntropyPersistError {
    /// Create entropy: {0}
    CreateEntropy(#[from] EntropyError),
    /// Virtio state: {0}
    VirtioState(#[from] VirtioStateError),
    /// Restore rate limiter: {0}
    RestoreRateLimiter(#[from] std::io::Error),
}

impl Persist<'_> for Entropy {
    type State = EntropyState;
    type ConstructorArgs = EntropyConstructorArgs;
    type Error = EntropyPersistError;

    fn save(&self) -> Self::State {
        EntropyState {
            virtio_state: VirtioDeviceState::from_device(self),
            rate_limiter_state: self.rate_limiter().save(),
            seed: self.seed(),
            rng_state: self.rng_state().clone(),
        }
    }

    fn restore(
        constructor_args: Self::ConstructorArgs,
        state: &Self::State,
    ) -> Result<Self, Self::Error> {
        let queues = state.virtio_state.build_queues_checked(
            &constructor_args.mem,
            VirtioDeviceType::Rng,
            RNG_NUM_QUEUES,
            FIRECRACKER_MAX_QUEUE_SIZE,
        )?;

        let rate_limiter = RateLimiter::restore((), &state.rate_limiter_state)?;
        let mut entropy = Entropy::new_with_queues(
            queues,
            rate_limiter,
            state.seed,
            Some(state.rng_state.clone()),
        )?;
        entropy.set_avail_features(state.virtio_state.avail_features);
        entropy.set_acked_features(state.virtio_state.acked_features);

        Ok(entropy)
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::devices::virtio::device::VirtioDevice;
    use crate::devices::virtio::rng::device::ENTROPY_DEV_ID;
    use crate::devices::virtio::test_utils::test::create_virtio_mem;

    #[test]
    fn test_persistence() {
        let entropy = Entropy::new(RateLimiter::default(), None).unwrap();

        let entropy_state = entropy.save();
        let serialized_data = bitcode::serialize(&entropy_state).unwrap();

        let guest_mem = create_virtio_mem();
        let restored_state = bitcode::deserialize(&serialized_data).unwrap();
        let restored =
            Entropy::restore(EntropyConstructorArgs { mem: guest_mem }, &restored_state).unwrap();

        assert_eq!(restored.device_type(), VirtioDeviceType::Rng);
        assert_eq!(restored.id(), ENTROPY_DEV_ID);
        assert!(!restored.is_activated());
        assert!(!entropy.is_activated());
        assert_eq!(restored.avail_features(), entropy.avail_features());
        assert_eq!(restored.acked_features(), entropy.acked_features());
        assert_eq!(restored.seed(), entropy.seed());
    }

    #[test]
    fn test_stream_continuity_across_snapshot() {
        use rand_chacha::rand_core::RngCore;

        // A restored device must continue the entropy stream exactly where the
        // snapshotted one left off — the snapshot carries the full RNG state.
        let entropy = Entropy::new(RateLimiter::default(), Some(7)).unwrap();
        let mut expected = [0u8; 128];
        entropy.rng_state().clone().fill_bytes(&mut expected);

        let state = entropy.save();
        let restored = Entropy::restore(
            EntropyConstructorArgs {
                mem: create_virtio_mem(),
            },
            &state,
        )
        .unwrap();

        let mut actual = [0u8; 128];
        restored.rng_state().clone().fill_bytes(&mut actual);
        assert_eq!(
            actual, expected,
            "restored device must resume the entropy stream at the snapshot position"
        );

        // And a snapshot taken after generating bytes must not replay them.
        let mut entropy = Entropy::new(RateLimiter::default(), Some(7)).unwrap();
        let mut sink = [0u8; 64];
        entropy.rng_state_mut().fill_bytes(&mut sink);
        let state_after = entropy.save();
        let restored_after = Entropy::restore(
            EntropyConstructorArgs {
                mem: create_virtio_mem(),
            },
            &state_after,
        )
        .unwrap();
        let mut resumed = [0u8; 64];
        restored_after
            .rng_state()
            .clone()
            .fill_bytes(&mut resumed);
        assert_eq!(
            resumed,
            &expected[64..],
            "snapshot after 64 generated bytes must resume at byte 64"
        );
    }
}
