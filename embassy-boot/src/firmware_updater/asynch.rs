use digest::Digest;
#[cfg(target_os = "none")]
use embassy_embedded_hal::flash::partition::Partition;
#[cfg(target_os = "none")]
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embedded_storage_async::nor_flash::NorFlash;

use super::FirmwareUpdaterConfig;
use crate::{FirmwareUpdaterError, State, BOOT_MAGIC, DFU_DETACH_MAGIC, PROGRESS_MAGIC, SWAP_MAGIC, REVERT_MAGIC};

/// FirmwareUpdater is an application API for interacting with the BootLoader without the ability to
/// 'mess up' the internal bootloader state
pub struct FirmwareUpdater<'d, DFU: NorFlash, STATE: NorFlash> {
    dfu: DFU,
    state: FirmwareState<'d, STATE>,
    last_erased_dfu_sector_index: Option<usize>,
}

#[cfg(target_os = "none")]
impl<'a, DFU: NorFlash, STATE: NorFlash>
    FirmwareUpdaterConfig<Partition<'a, NoopRawMutex, DFU>, Partition<'a, NoopRawMutex, STATE>>
{
    /// Create a firmware updater config from the flash and address symbols defined in the linkerfile
    pub fn from_linkerfile(
        dfu_flash: &'a embassy_sync::mutex::Mutex<NoopRawMutex, DFU>,
        state_flash: &'a embassy_sync::mutex::Mutex<NoopRawMutex, STATE>,
    ) -> Self {
        extern "C" {
            static __bootloader_state_start: u32;
            static __bootloader_state_end: u32;
            static __bootloader_dfu_start: u32;
            static __bootloader_dfu_end: u32;
        }

        let dfu = unsafe {
            let start = &__bootloader_dfu_start as *const u32 as u32;
            let end = &__bootloader_dfu_end as *const u32 as u32;
            trace!("DFU: 0x{:x} - 0x{:x}", start, end);

            Partition::new(dfu_flash, start, end - start)
        };
        let state = unsafe {
            let start = &__bootloader_state_start as *const u32 as u32;
            let end = &__bootloader_state_end as *const u32 as u32;
            trace!("STATE: 0x{:x} - 0x{:x}", start, end);

            Partition::new(state_flash, start, end - start)
        };

        Self { dfu, state }
    }
}

impl<'d, DFU: NorFlash, STATE: NorFlash> FirmwareUpdater<'d, DFU, STATE> {
    /// Create a firmware updater instance with partition ranges for the update and state partitions.
    pub fn new(config: FirmwareUpdaterConfig<DFU, STATE>, aligned: &'d mut [u8]) -> Self {
        Self {
            dfu: config.dfu,
            state: FirmwareState::new(config.state, aligned),
            last_erased_dfu_sector_index: None,
        }
    }

    /// Obtain the current state.
    ///
    /// This is useful to check if the bootloader has just done a swap, in order
    /// to do verifications and self-tests of the new image before calling
    /// `mark_booted`.
    pub async fn get_state(&mut self) -> Result<State, FirmwareUpdaterError> {
        self.state.get_state().await
    }

    /// Verify the DFU given a public key. If there is an error then DO NOT
    /// proceed with updating the firmware as it must be signed with a
    /// corresponding private key (otherwise it could be malicious firmware).
    ///
    /// Mark to trigger firmware swap on next boot if verify succeeds.
    ///
    /// If the "ed25519-salty" feature is set (or another similar feature) then the signature is expected to have
    /// been generated from a SHA-512 digest of the firmware bytes.
    ///
    /// If no signature feature is set then this method will always return a
    /// signature error.
    #[cfg(feature = "_verify")]
    pub async fn verify_and_mark_updated(
        &mut self,
        _public_key: &[u8; 32],
        _signature: &[u8; 64],
        _update_len: u32,
    ) -> Result<(), FirmwareUpdaterError> {
        assert!(_update_len <= self.dfu.capacity() as u32);

        self.state.verify_booted().await?;

        #[cfg(feature = "ed25519-dalek")]
        {
            use ed25519_dalek::{Signature, SignatureError, Verifier, VerifyingKey};

            use crate::digest_adapters::ed25519_dalek::Sha512;

            let into_signature_error = |e: SignatureError| FirmwareUpdaterError::Signature(e.into());

            let public_key = VerifyingKey::from_bytes(_public_key).map_err(into_signature_error)?;
            let signature = Signature::from_bytes(_signature);

            let mut chunk_buf = [0; 2];
            let mut message = [0; 64];
            self.hash::<Sha512>(_update_len, &mut chunk_buf, &mut message).await?;

            public_key.verify(&message, &signature).map_err(into_signature_error)?;
            return self.state.mark_updated().await;
        }
        #[cfg(feature = "ed25519-salty")]
        {
            use salty::{PublicKey, Signature};

            use crate::digest_adapters::salty::Sha512;

            fn into_signature_error<E>(_: E) -> FirmwareUpdaterError {
                FirmwareUpdaterError::Signature(signature::Error::default())
            }

            let public_key = PublicKey::try_from(_public_key).map_err(into_signature_error)?;
            let signature = Signature::try_from(_signature).map_err(into_signature_error)?;

            let mut message = [0; 64];
            let mut chunk_buf = [0; 2];
            self.hash::<Sha512>(_update_len, &mut chunk_buf, &mut message).await?;

            let r = public_key.verify(&message, &signature);
            trace!(
                "Verifying with public key {}, signature {} and message {} yields ok: {}",
                public_key.to_bytes(),
                signature.to_bytes(),
                message,
                r.is_ok()
            );
            r.map_err(into_signature_error)?;
            return self.state.mark_updated().await;
        }
        #[cfg(not(any(feature = "ed25519-dalek", feature = "ed25519-salty")))]
        {
            Err(FirmwareUpdaterError::Signature(signature::Error::new()))
        }
    }

    /// Verify the update in DFU with any digest.
    pub async fn hash<D: Digest>(
        &mut self,
        update_len: u32,
        chunk_buf: &mut [u8],
        output: &mut [u8],
    ) -> Result<(), FirmwareUpdaterError> {
        let mut digest = D::new();
        for offset in (0..update_len).step_by(chunk_buf.len()) {
            self.dfu.read(offset, chunk_buf).await?;
            let len = core::cmp::min((update_len - offset) as usize, chunk_buf.len());
            digest.update(&chunk_buf[..len]);
        }
        output.copy_from_slice(digest.finalize().as_slice());
        Ok(())
    }

    /// Read a slice of data from the DFU storage peripheral, starting the read
    /// operation at the given address offset, and reading `buf.len()` bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if the arguments are not aligned or out of bounds.
    pub async fn read_dfu(&mut self, offset: u32, buf: &mut [u8]) -> Result<(), FirmwareUpdaterError> {
        self.dfu.read(offset, buf).await?;
        Ok(())
    }

    /// Mark to trigger firmware swap on next boot.
    #[cfg(not(feature = "_verify"))]
    pub async fn mark_updated(&mut self) -> Result<(), FirmwareUpdaterError> {
        self.state.mark_updated().await
    }

    /// Mark to trigger USB DFU on next boot.
    pub async fn mark_dfu(&mut self) -> Result<(), FirmwareUpdaterError> {
        self.state.verify_booted().await?;
        self.state.mark_dfu().await
    }

    /// Mark firmware boot successful and stop rollback on reset.
    pub async fn mark_booted(&mut self) -> Result<(), FirmwareUpdaterError> {
        self.state.mark_booted().await
    }

    /// Writes firmware data to the device.
    ///
    /// This function writes the given data to the firmware area starting at the specified offset.
    /// It handles sector erasures and data writes while verifying the device is in a proper state
    /// for firmware updates. The function ensures that only unerased sectors are erased before
    /// writing and efficiently handles the writing process across sector boundaries and in
    /// various configurations (data size, sector size, etc.).
    ///
    /// # Arguments
    ///
    /// * `offset` - The starting offset within the firmware area where data writing should begin.
    /// * `data` - A slice of bytes representing the firmware data to be written. It must be a
    /// multiple of NorFlash WRITE_SIZE.
    ///
    /// # Returns
    ///
    /// A `Result<(), FirmwareUpdaterError>` indicating the success or failure of the write operation.
    ///
    /// # Errors
    ///
    /// This function will return an error if:
    ///
    /// - The device is not in a proper state to receive firmware updates (e.g., not booted).
    /// - There is a failure erasing a sector before writing.
    /// - There is a failure writing data to the device.
    pub async fn write_firmware(&mut self, offset: usize, data: &[u8]) -> Result<(), FirmwareUpdaterError> {
        // Make sure we are running a booted firmware to avoid reverting to a bad state.
        self.state.verify_booted().await?;

        // Initialize variables to keep track of the remaining data and the current offset.
        let mut remaining_data = data;
        let mut offset = offset;

        // Continue writing as long as there is data left to write.
        while !remaining_data.is_empty() {
            // Compute the current sector and its boundaries.
            let current_sector = offset / DFU::ERASE_SIZE;
            let sector_start = current_sector * DFU::ERASE_SIZE;
            let sector_end = sector_start + DFU::ERASE_SIZE;
            // Determine if the current sector needs to be erased before writing.
            let need_erase = self
                .last_erased_dfu_sector_index
                .map_or(true, |last_erased_sector| current_sector != last_erased_sector);

            // If the sector needs to be erased, erase it and update the last erased sector index.
            if need_erase {
                self.dfu.erase(sector_start as u32, sector_end as u32).await?;
                self.last_erased_dfu_sector_index = Some(current_sector);
            }

            // Calculate the size of the data chunk that can be written in the current iteration.
            let write_size = core::cmp::min(remaining_data.len(), sector_end - offset);
            // Split the data to get the current chunk to be written and the remaining data.
            let (data_chunk, rest) = remaining_data.split_at(write_size);

            // Write the current data chunk.
            self.dfu.write(offset as u32, data_chunk).await?;

            // Update the offset and remaining data for the next iteration.
            remaining_data = rest;
            offset += write_size;
        }

        Ok(())
    }

    /// Prepare for an incoming DFU update by erasing the entire DFU area and
    /// returning its `Partition`.
    ///
    /// Using this instead of `write_firmware` allows for an optimized API in
    /// exchange for added complexity.
    pub async fn prepare_update(&mut self) -> Result<&mut DFU, FirmwareUpdaterError> {
        self.state.verify_booted().await?;
        self.dfu.erase(0, self.dfu.capacity() as u32).await?;

        Ok(&mut self.dfu)
    }
}

/// Manages the state partition of the firmware update.
///
/// Can be used standalone for more fine grained control, or as part of the updater.
pub struct FirmwareState<'d, STATE> {
    state: STATE,
    aligned: &'d mut [u8],
}

impl<'d, STATE: NorFlash> FirmwareState<'d, STATE> {
    /// Create a firmware state instance from a FirmwareUpdaterConfig with a buffer for magic content and state partition.
    ///
    /// # Safety
    ///
    /// The `aligned` buffer must have a size of STATE::WRITE_SIZE, and follow the alignment rules for the flash being read from
    /// and written to.
    pub fn from_config<DFU: NorFlash>(config: FirmwareUpdaterConfig<DFU, STATE>, aligned: &'d mut [u8]) -> Self {
        Self::new(config.state, aligned)
    }

    /// Create a firmware state instance with a buffer for magic content and state partition.
    ///
    /// # Safety
    ///
    /// The `aligned` buffer must have a size of maximum of STATE::WRITE_SIZE and STATE::READ_SIZE,
    /// and follow the alignment rules for the flash being read from and written to.
    pub fn new(state: STATE, aligned: &'d mut [u8]) -> Self {
        assert_eq!(aligned.len(), STATE::WRITE_SIZE.max(STATE::READ_SIZE));
        // assert if any byte of STATE::ERASE_VALUE is one of our magic bytes
        // assert_eq!(STATE::ERASE_VALUE, &[]);
        assert!(STATE::ERASE_VALUE
            .iter()
            .any(|&b| !(b == PROGRESS_MAGIC || b == BOOT_MAGIC || b == SWAP_MAGIC || b == DFU_DETACH_MAGIC || b == REVERT_MAGIC )));
        Self { state, aligned }
    }

    // Make sure we are running a booted firmware to avoid reverting to a bad state.
    async fn verify_booted(&mut self) -> Result<(), FirmwareUpdaterError> {
        let state = self.get_state().await?;
        if state == State::Boot || state == State::DfuDetach || state == State::Revert {
            Ok(())
        } else {
            Err(FirmwareUpdaterError::BadState)
        }
    }

    /// Obtain the current state.
    ///
    /// This is useful to check if the bootloader has just done a swap, in order
    /// to do verifications and self-tests of the new image before calling
    /// `mark_booted`.
    pub async fn get_state(&mut self) -> Result<State, FirmwareUpdaterError> {
        self.state.read(0, &mut self.aligned).await?;
        Ok(State::from(&self.aligned[..STATE::WRITE_SIZE]))
    }

    /// Mark to trigger firmware swap on next boot.
    pub async fn mark_updated(&mut self) -> Result<(), FirmwareUpdaterError> {
        self.set_magic(SWAP_MAGIC).await
    }

    /// Mark to trigger USB DFU on next boot.
    pub async fn mark_dfu(&mut self) -> Result<(), FirmwareUpdaterError> {
        self.set_magic(DFU_DETACH_MAGIC).await
    }

    /// Mark firmware boot successful and stop rollback on reset.
    pub async fn mark_booted(&mut self) -> Result<(), FirmwareUpdaterError> {
        self.set_magic(BOOT_MAGIC).await
    }

    async fn set_magic(&mut self, magic: u8) -> Result<(), FirmwareUpdaterError> {
        self.state.read(0, &mut self.aligned).await?;

        if self.aligned[..STATE::WRITE_SIZE].iter().any(|&b| b != magic) {
            // Read progress validity
            if STATE::READ_SIZE <= 2 * STATE::WRITE_SIZE {
                self.state.read(STATE::WRITE_SIZE as u32, &mut self.aligned).await?;
            } else {
                self.aligned.rotate_left(STATE::WRITE_SIZE);
            }

            if self.aligned[..STATE::WRITE_SIZE] != *STATE::ERASE_VALUE {
                // The current progress validity marker is invalid
            } else {
                // Invalidate progress
                self.aligned.fill(PROGRESS_MAGIC);
                self.state
                    .write(STATE::WRITE_SIZE as u32, &self.aligned[..STATE::WRITE_SIZE])
                    .await?;
            }

            // Clear magic and progress
            self.state.erase(0, self.state.capacity() as u32).await?;

            // Set magic
            self.aligned.fill(magic);
            self.state.write(0, &self.aligned[..STATE::WRITE_SIZE]).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use embassy_embedded_hal::flash::partition::Partition;
    use embassy_sync::blocking_mutex::raw::NoopRawMutex;
    use embassy_sync::mutex::Mutex;
    use futures::executor::block_on;
    use sha1::{Digest, Sha1};

    use super::*;
    use crate::mem_flash::MemFlash;

    #[test]
    fn can_verify_sha1() {
        let flash = Mutex::<NoopRawMutex, _>::new(MemFlash::<131072, 4096, 8>::default());
        let state = Partition::new(&flash, 0, 4096);
        let dfu = Partition::new(&flash, 65536, 65536);
        let mut aligned = [0; 8];

        let update = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let mut to_write = [0; 4096];
        to_write[..7].copy_from_slice(update.as_slice());

        let mut updater = FirmwareUpdater::new(FirmwareUpdaterConfig { dfu, state }, &mut aligned);
        block_on(updater.write_firmware(0, to_write.as_slice())).unwrap();
        let mut chunk_buf = [0; 2];
        let mut hash = [0; 20];
        block_on(updater.hash::<Sha1>(update.len() as u32, &mut chunk_buf, &mut hash)).unwrap();

        assert_eq!(Sha1::digest(update).as_slice(), hash);
    }

    #[test]
    fn can_verify_sha1_sector_bigger_than_chunk() {
        let flash = Mutex::<NoopRawMutex, _>::new(MemFlash::<131072, 4096, 8>::default());
        let state = Partition::new(&flash, 0, 4096);
        let dfu = Partition::new(&flash, 65536, 65536);
        let mut aligned = [0; 8];

        let update = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let mut to_write = [0; 4096];
        to_write[..7].copy_from_slice(update.as_slice());

        let mut updater = FirmwareUpdater::new(FirmwareUpdaterConfig { dfu, state }, &mut aligned);
        let mut offset = 0;
        for chunk in to_write.chunks(1024) {
            block_on(updater.write_firmware(offset, chunk)).unwrap();
            offset += chunk.len();
        }
        let mut chunk_buf = [0; 2];
        let mut hash = [0; 20];
        block_on(updater.hash::<Sha1>(update.len() as u32, &mut chunk_buf, &mut hash)).unwrap();

        assert_eq!(Sha1::digest(update).as_slice(), hash);
    }

    #[test]
    fn can_verify_sha1_sector_smaller_than_chunk() {
        let flash = Mutex::<NoopRawMutex, _>::new(MemFlash::<131072, 1024, 8>::default());
        let state = Partition::new(&flash, 0, 4096);
        let dfu = Partition::new(&flash, 65536, 65536);
        let mut aligned = [0; 8];

        let update = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let mut to_write = [0; 4096];
        to_write[..7].copy_from_slice(update.as_slice());

        let mut updater = FirmwareUpdater::new(FirmwareUpdaterConfig { dfu, state }, &mut aligned);
        let mut offset = 0;
        for chunk in to_write.chunks(2048) {
            block_on(updater.write_firmware(offset, chunk)).unwrap();
            offset += chunk.len();
        }
        let mut chunk_buf = [0; 2];
        let mut hash = [0; 20];
        block_on(updater.hash::<Sha1>(update.len() as u32, &mut chunk_buf, &mut hash)).unwrap();

        assert_eq!(Sha1::digest(update).as_slice(), hash);
    }

    #[test]
    fn can_verify_sha1_cross_sector_boundary() {
        let flash = Mutex::<NoopRawMutex, _>::new(MemFlash::<131072, 1024, 8>::default());
        let state = Partition::new(&flash, 0, 4096);
        let dfu = Partition::new(&flash, 65536, 65536);
        let mut aligned = [0; 8];

        let update = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let mut to_write = [0; 4096];
        to_write[..7].copy_from_slice(update.as_slice());

        let mut updater = FirmwareUpdater::new(FirmwareUpdaterConfig { dfu, state }, &mut aligned);
        let mut offset = 0;
        for chunk in to_write.chunks(896) {
            block_on(updater.write_firmware(offset, chunk)).unwrap();
            offset += chunk.len();
        }
        let mut chunk_buf = [0; 2];
        let mut hash = [0; 20];
        block_on(updater.hash::<Sha1>(update.len() as u32, &mut chunk_buf, &mut hash)).unwrap();

        assert_eq!(Sha1::digest(update).as_slice(), hash);
    }
}
