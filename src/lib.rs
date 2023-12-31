#![forbid(unsafe_code, unstable_features)]
#![warn(
    missing_docs,
    clippy::missing_docs_in_private_items,
    clippy::nursery,
    clippy::pedantic
)]
#![allow(
    clippy::too_many_arguments,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::module_name_repetitions
)]
#![doc =include_str!("../README.md")]

pub use error::{Error, Result};

use std::{
    fmt::{self, Debug, Formatter},
    io::{Cursor, Write},
    sync::Arc,
};

#[macro_use]
extern crate tracing;

#[cfg(feature = "strum")]
pub use strum::IntoEnumIterator;

mod error;
pub mod motor;
mod protocol;
pub mod sensor;
mod socket;
pub mod system;

#[cfg(feature = "usb")]
pub use socket::usb::Usb;

#[cfg(feature = "bluetooth")]
pub use socket::bluetooth::Bluetooth;

use motor::{OutMode, OutPort, OutputState, RegulationMode, RunState};
use protocol::{Opcode, Packet};
use sensor::{InPort, InputValues, SensorMode, SensorType};
use socket::Socket;
use system::{
    BufType, DeviceInfo, FileHandle, FindFileHandle, FwVersion, ModuleHandle,
};

/// Maximum length of a USB message
pub const MAX_MESSAGE_LEN: usize = 58;
/// Length of the brick name field
const MAX_NAME_LEN: usize = 15;
/// Largest inbox ID for inter-brick messaging
pub const MAX_INBOX_ID: u8 = 19;

/// Module ID of the display (tested on the NBC enhanced firmware, may
/// differ for the official LEGO firmware)
const MOD_DISPLAY: u32 = 0xa0001;
/// Offset of the display data into the display iomap struct; consult
/// the firmware source for details
const DISPLAY_DATA_OFFSET: u16 = 119;
/// Width of NXT LCD screen in pixels
pub const DISPLAY_WIDTH: usize = 100;
/// Height of NXT LCD screen in pixels
pub const DISPLAY_HEIGHT: usize = 64;
/// Total number of LCD pixels
pub const DISPLAY_DATA_LEN: usize = DISPLAY_WIDTH * DISPLAY_HEIGHT / 8;
/// Chunk size to use when requesting display data from NXT. Due to the
/// packet size restriction the display must be refreshed in chunks.
const DISPLAY_DATA_CHUNK_SIZE: u16 = 32;
/// Number of chunks required to retrieve the complete display data
#[allow(clippy::cast_possible_truncation)]
const DISPLAY_NUM_CHUNKS: u16 =
    DISPLAY_DATA_LEN as u16 / DISPLAY_DATA_CHUNK_SIZE;

/// Main interface to this crate, an `NXT` represents a connection to a
/// programmable brick.
#[derive(Clone)]
pub struct Nxt {
    /// Socket device, e.g. USB or Bluetooth
    device: Arc<dyn Socket + Send + Sync>,
    /// Name of the brick
    name: String,
}

impl Debug for Nxt {
    fn fmt(&self, fmt: &mut Formatter) -> fmt::Result {
        fmt.debug_struct("NXT")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Nxt {
    /// Search for plugged-in NXT devices and establish a connection to
    /// the first one
    #[cfg(feature = "usb")]
    pub async fn first_usb() -> Result<Self> {
        let device = socket::usb::Usb::first()?;
        Self::init(device).await
    }

    /// Connect to all plugged-in NXT bricks and return them in a `Vec`
    #[cfg(feature = "usb")]
    pub async fn all_usb() -> Result<Vec<Self>> {
        let devices = socket::usb::Usb::all()?;
        futures::future::try_join_all(devices.into_iter().map(Self::init)).await
    }

    /// Initialise an NXT struct from the given device
    pub async fn init<D: Socket + Send + Sync + 'static>(
        device: D,
    ) -> Result<Self> {
        debug!("Initialise NXT from {} device", std::any::type_name::<D>());
        let mut nxt = Self {
            device: Arc::new(device),
            name: String::new(),
        };
        let info = nxt.get_device_info().await?;
        debug!("Connected device is named `{}`", info.name);
        nxt.name = info.name;
        Ok(nxt)
    }

    /// Return the name of the NXT brick
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Send the provided packet an optionally check the response status.
    /// Use this API if there's no useful data in the reply beyond the
    /// status field
    async fn send(&self, pkt: &Packet, check_status: bool) -> Result<()> {
        let mut buf = [0; 64];
        let serialised = pkt.serialise(&mut buf)?;

        let written = self.device.send(serialised).await?;
        if written == serialised.len() {
            if check_status {
                let _recv = self.recv(pkt.opcode).await?;
            }
            Ok(())
        } else {
            Err(Error::Write)
        }
    }

    /// Read an incoming reply packet and verify that its opcode matches
    /// the expected value
    async fn recv(&self, opcode: Opcode) -> Result<Packet> {
        let mut buf = [0; 64];
        let buf = self.device.recv(&mut buf).await?;

        let mut recv = Packet::parse(buf)?;
        recv.check_status()?;
        if recv.opcode == opcode {
            Ok(recv)
        } else {
            Err(Error::ReplyMismatch)
        }
    }

    /// Send the provided packet and read the response. Use this API
    /// when the reply is expected to contain useful data, e.g. sensor
    /// values
    async fn send_recv(&self, pkt: &Packet) -> Result<Packet> {
        self.send(pkt, false).await?;
        self.recv(pkt.opcode).await
    }

    /// Convenience function to retrieve the contents of the LCD screen.
    /// The data is in a slightly odd format; see
    /// [`system::display_data_to_raster`] for details.
    pub async fn get_display_data(&self) -> Result<[u8; DISPLAY_DATA_LEN]> {
        let out = [0; DISPLAY_DATA_LEN];
        let mut cur = Cursor::new(out);
        for chunk_idx in 0..DISPLAY_NUM_CHUNKS {
            let data = self
                .read_io_map(
                    MOD_DISPLAY,
                    DISPLAY_DATA_OFFSET + chunk_idx * DISPLAY_DATA_CHUNK_SIZE,
                    DISPLAY_DATA_CHUNK_SIZE,
                )
                .await?;
            assert_eq!(data.len(), DISPLAY_DATA_CHUNK_SIZE.into());
            cur.write_all(&data)?;
        }

        Ok(cur.into_inner())
    }

    /// Retrieve the current battery level, in mV
    pub async fn get_battery_level(&self) -> Result<u16> {
        let pkt = Packet::new(Opcode::DirectGetBattLevel);
        let mut recv = self.send_recv(&pkt).await?;
        recv.read_u16()
    }

    /// Read firmware versions from the NXT brick
    pub async fn get_firmware_version(&self) -> Result<FwVersion> {
        let pkt = Packet::new(Opcode::SystemVersions);
        let mut recv = self.send_recv(&pkt).await?;
        let prot_min = recv.read_u8()?;
        let prot_maj = recv.read_u8()?;
        let fw_min = recv.read_u8()?;
        let fw_maj = recv.read_u8()?;
        Ok(FwVersion {
            prot: (prot_maj, prot_min),
            fw: (fw_maj, fw_min),
        })
    }

    /// Start running the program with the specified name. Returns an
    /// `ERR_RC_ILLEGAL_VAL` error if the file does not exist.
    pub async fn start_program(&self, name: &str) -> Result<()> {
        let mut pkt = Packet::new(Opcode::DirectStartProgram);
        pkt.push_filename(name)?;
        self.send(&pkt, true).await
    }

    /// Stop the currently executing program. Returns an `ERR_NO_PROG`
    /// error if there is no program running.
    pub async fn stop_program(&self) -> Result<()> {
        let pkt = Packet::new(Opcode::DirectStopProgram);
        self.send(&pkt, true).await
    }

    /// Play the specified sound file. Returns an `ERR_RC_ILLEGAL_VAL`
    /// if the file does not exist
    pub async fn play_sound(&self, file: &str, loop_: bool) -> Result<()> {
        let mut pkt = Packet::new(Opcode::DirectPlaySoundFile);
        pkt.push_bool(loop_);
        pkt.push_filename(file)?;
        self.send(&pkt, true).await
    }

    /// Play the specified tone for the given duration.
    pub async fn play_tone(&self, freq: u16, duration_ms: u16) -> Result<()> {
        let mut pkt = Packet::new(Opcode::DirectPlayTone);
        pkt.push_u16(freq);
        pkt.push_u16(duration_ms);
        self.send(&pkt, true).await
    }

    /// Set the output state for the given individual or compound port
    pub async fn set_output_state(
        &self,
        port: OutPort,
        power: i8,
        mode: OutMode,
        regulation_mode: RegulationMode,
        turn_ratio: i8,
        run_state: RunState,
        tacho_limit: u32,
    ) -> Result<()> {
        let mut pkt = Packet::new(Opcode::DirectSetOutState);
        pkt.push_u8(port as u8);
        pkt.push_i8(power);
        pkt.push_u8(mode.0);
        pkt.push_u8(regulation_mode as u8);
        pkt.push_i8(turn_ratio);
        pkt.push_u8(run_state as u8);
        pkt.push_u32(tacho_limit);
        self.send(&pkt, true).await
    }

    /// Set the given input to the specified mode
    pub async fn set_input_mode(
        &self,
        port: InPort,
        sensor_type: SensorType,
        sensor_mode: SensorMode,
    ) -> Result<()> {
        let mut pkt = Packet::new(Opcode::DirectSetInMode);
        pkt.push_u8(port as u8);
        pkt.push_u8(sensor_type as u8);
        pkt.push_u8(sensor_mode as u8);
        self.send(&pkt, true).await
    }

    /// Retrieve the state of the specified output. Returns an
    /// `ERR_RC_ILLEGAL_VAL` if the port is not a valid single port
    /// specification
    pub async fn get_output_state(&self, port: OutPort) -> Result<OutputState> {
        let mut pkt = Packet::new(Opcode::DirectGetOutState);
        pkt.push_u8(port as u8);
        self.send(&pkt, false).await?;
        let mut recv = self.recv(Opcode::DirectGetOutState).await?;
        let port = recv.read_u8()?.try_into()?;
        let power = recv.read_i8()?;
        let mode = recv.read_u8()?.into();
        let regulation_mode = recv.read_u8()?.try_into()?;
        let turn_ratio = recv.read_i8()?;
        let run_state = recv.read_u8()?.try_into()?;
        let tacho_limit = recv.read_u32()?;
        let tacho_count = recv.read_i32()?;
        let block_tacho_count = recv.read_i32()?;
        let rotation_count = recv.read_i32()?;

        Ok(OutputState {
            port,
            power,
            mode,
            regulation_mode,
            turn_ratio,
            run_state,
            tacho_limit,
            tacho_count,
            block_tacho_count,
            rotation_count,
        })
    }

    /// Retrieve the state of the specified input port
    pub async fn get_input_values(&self, port: InPort) -> Result<InputValues> {
        let mut pkt = Packet::new(Opcode::DirectGetInVals);
        pkt.push_u8(port as u8);
        let mut recv = self.send_recv(&pkt).await?;
        // hdr>>  s  p  v  c  ty mo  raw>>  norm>  sc>>  cal>>
        // [2, 7, 0, 0, 1, 0, 1, 20, ff, 3, ff, 3, 0, 0, ff, 3]
        let port = recv.read_u8()?.try_into()?;
        let valid = recv.read_bool()?;
        let calibrated = recv.read_bool()?;
        let sensor_type = recv.read_u8()?.try_into()?;
        let sensor_mode = recv.read_u8()?.try_into()?;
        let raw_value = recv.read_u16()?;
        let normalised_value = recv.read_u16()?;
        let scaled_value = recv.read_i16()?;
        let calibrated_value = recv.read_i16()?;

        Ok(InputValues {
            port,
            valid,
            calibrated,
            sensor_type,
            sensor_mode,
            raw_value,
            normalised_value,
            scaled_value,
            calibrated_value,
        })
    }

    /// Reset the scaled value of the spcified input port, e.g. clears
    /// the edge or pulse counter.
    pub async fn reset_input_scaled_value(&self, port: InPort) -> Result<()> {
        let mut pkt = Packet::new(Opcode::DirectResetInVal);
        pkt.push_u8(port as u8);
        self.send(&pkt, true).await
    }

    /// Write a message to the specified inbox. Returns an error if the
    /// inbox ID is greater than [`MAX_INBOX_ID`] or of the message is
    /// longer than [`MAX_MESSAGE_LEN`] bytes
    pub async fn message_write(&self, inbox: u8, message: &[u8]) -> Result<()> {
        if inbox > MAX_INBOX_ID {
            return Err(Error::Serialise("Invalid mailbox ID"));
        }
        if message.len() > MAX_MESSAGE_LEN {
            return Err(Error::Serialise("Message too long (max 58 bytes)"));
        }

        let mut pkt = Packet::new(Opcode::DirectMessageWrite);
        pkt.push_u8(inbox);
        // data length has already been checked
        #[allow(clippy::cast_possible_truncation)]
        pkt.push_u8(message.len() as u8 + 1);
        pkt.push_slice(message);
        pkt.push_u8(0);
        self.send(&pkt, true).await
    }

    /// Reset the motor position counter. Returns an `ERR_RC_ILLEGAL_VAL`
    /// if the port is not an individual port specification.
    ///
    /// * `relative`:
    ///   * `TRUE`: reset position relative to last motor control block
    ///   * `FALSE`: reset position relative to start of last program
    pub async fn reset_motor_position(
        &self,
        port: OutPort,
        relative: bool,
    ) -> Result<()> {
        let mut pkt = Packet::new(Opcode::DirectResetPosition);
        pkt.push_u8(port as u8);
        pkt.push_bool(relative);
        self.send(&pkt, true).await
    }

    /// Stop playing the current sound file, if any
    pub async fn stop_sound_playback(&self) -> Result<()> {
        let pkt = Packet::new(Opcode::DirectStopSound);
        self.send(&pkt, true).await
    }

    /// Reset the sleep timer and return the sleep timeout
    pub async fn keep_alive(&self) -> Result<u32> {
        let pkt = Packet::new(Opcode::DirectKeepAlive);
        self.send(&pkt, false).await?;
        let mut recv = self.recv(Opcode::DirectKeepAlive).await?;
        recv.read_u32()
    }

    /// Retrieve the status of the specified low speed port
    pub async fn ls_get_status(&self, port: InPort) -> Result<u8> {
        let mut pkt = Packet::new(Opcode::DirectLsGetStatus);
        pkt.push_u8(port as u8);
        self.send(&pkt, false).await?;
        let mut recv = self.recv(Opcode::DirectLsGetStatus).await?;
        recv.read_u8()
    }

    /// Write the provided data to the low speed bus on the given port
    /// and read the  specified number of bytes in response
    pub async fn ls_write(
        &self,
        port: InPort,
        tx_data: &[u8],
        rx_bytes: u8,
    ) -> Result<()> {
        // unsure what limit should be here, go with max packet size for
        // now
        if tx_data.len() > MAX_MESSAGE_LEN {
            return Err(Error::Serialise("Data too long"));
        }

        let mut pkt = Packet::new(Opcode::DirectLsWrite);
        pkt.push_u8(port as u8);
        // data length has already been checked
        #[allow(clippy::cast_possible_truncation)]
        pkt.push_u8(tx_data.len() as u8);
        pkt.push_u8(rx_bytes);
        pkt.push_slice(tx_data);
        self.send(&pkt, true).await
    }

    /// Read data from the low speed port
    pub async fn ls_read(&self, port: InPort) -> Result<Vec<u8>> {
        let mut pkt = Packet::new(Opcode::DirectLsRead);
        pkt.push_u8(port as u8);
        self.send(&pkt, false).await?;
        let mut recv = self.recv(Opcode::DirectLsRead).await?;
        let len = recv.read_u8()?;
        let data = recv.read_slice(len as usize)?;
        Ok(data.to_vec())
    }

    /// Get the name of the currently running program. Returns
    /// `ERR_NO_PROG` if there is no program currently running
    pub async fn get_current_program_name(&self) -> Result<String> {
        let pkt = Packet::new(Opcode::DirectGetCurrProgram);
        self.send(&pkt, false).await?;
        let mut recv = self.recv(Opcode::DirectGetCurrProgram).await?;
        recv.read_filename()
    }

    /// Read a message from the specified mailbox
    pub async fn message_read(
        &self,
        remote_inbox: u8,
        local_inbox: u8,
        remove: bool,
    ) -> Result<Vec<u8>> {
        let mut pkt = Packet::new(Opcode::DirectMessageRead);
        pkt.push_u8(remote_inbox);
        pkt.push_u8(local_inbox);
        pkt.push_bool(remove);
        self.send(&pkt, false).await?;
        let mut recv = self.recv(Opcode::DirectMessageRead).await?;
        let _local_inbox = recv.read_u8()?;
        let len = recv.read_u8()?;
        let data = recv.read_slice(len as usize)?;
        Ok(data.to_vec())
    }

    /// Open the specified file for writing and return its handle
    pub async fn file_open_write(
        &self,
        name: &str,
        len: u32,
    ) -> Result<FileHandle> {
        let mut pkt = Packet::new(Opcode::SystemOpenwrite);
        pkt.push_filename(name)?;
        pkt.push_u32(len);
        let mut recv = self.send_recv(&pkt).await?;
        let handle = recv.read_u8()?;
        Ok(FileHandle { handle, len })
    }

    /// Write the provided data to the previously opened file
    pub async fn file_write(
        &self,
        handle: &FileHandle,
        data: &[u8],
    ) -> Result<u32> {
        let mut pkt = Packet::new(Opcode::SystemWrite);
        pkt.push_u8(handle.handle);
        pkt.push_slice(data);
        let mut recv = self.send_recv(&pkt).await?;
        let _handle = recv.read_u8()?;
        recv.read_u32()
    }

    /// Open the specified file in `write data` mode and return its handle
    pub async fn file_open_write_data(
        &self,
        name: &str,
        len: u32,
    ) -> Result<FileHandle> {
        let mut pkt = Packet::new(Opcode::SystemOpenwritedata);
        pkt.push_filename(name)?;
        pkt.push_u32(len);
        let mut recv = self.send_recv(&pkt).await?;
        let handle = recv.read_u8()?;
        Ok(FileHandle { handle, len })
    }

    /// Open the specified file in `append` mode and return its handle
    pub async fn file_open_append_data(
        &self,
        name: &str,
    ) -> Result<FileHandle> {
        let mut pkt = Packet::new(Opcode::SystemOpenappenddata);
        pkt.push_filename(name)?;
        let mut recv = self.send_recv(&pkt).await?;
        let handle = recv.read_u8()?;
        let len = recv.read_u32()?;
        Ok(FileHandle { handle, len })
    }

    /// Close the specified file handle
    pub async fn file_close(&self, handle: &FileHandle) -> Result<()> {
        let mut pkt = Packet::new(Opcode::SystemClose);
        pkt.push_u8(handle.handle);
        self.send(&pkt, true).await
    }

    /// Open the specified file for reading and return its handle
    pub async fn file_open_read(&self, name: &str) -> Result<FileHandle> {
        let mut pkt = Packet::new(Opcode::SystemOpenread);
        pkt.push_filename(name)?;
        let mut recv = self.send_recv(&pkt).await?;
        let handle = recv.read_u8()?;
        let len = recv.read_u32()?;
        Ok(FileHandle { handle, len })
    }

    /// Read data from the previously opened file
    pub async fn file_read(
        &self,
        handle: &FileHandle,
        len: u32,
    ) -> Result<Vec<u8>> {
        let mut pkt = Packet::new(Opcode::SystemOpenread);
        pkt.push_u8(handle.handle);
        pkt.push_u32(len);
        let mut recv = self.send_recv(&pkt).await?;
        let _handle = recv.read_u8()?;
        let len = recv.read_u8()?;
        let data = recv.read_slice(len as usize)?;
        Ok(data.to_vec())
    }

    /// Delete the named file
    pub async fn file_delete(&self, name: &str) -> Result<()> {
        let mut pkt = Packet::new(Opcode::SystemDelete);
        pkt.push_filename(name)?;
        self.send(&pkt, true).await
    }

    /// Search for a file matching the specified pattern and return a
    /// handle to the search state
    pub async fn file_find_first(
        &self,
        pattern: &str,
    ) -> Result<FindFileHandle> {
        let mut pkt = Packet::new(Opcode::SystemFindfirst);
        pkt.push_filename(pattern)?;
        let mut recv = self.send_recv(&pkt).await?;
        let handle = recv.read_u8()?;
        let name = recv.read_filename()?;
        let len = recv.read_u32()?;
        Ok(FindFileHandle { handle, name, len })
    }

    /// Take a search handle and return the next match, or an error if
    /// there are no further matches
    pub async fn file_find_next(
        &self,
        handle: &FindFileHandle,
    ) -> Result<FindFileHandle> {
        let mut pkt = Packet::new(Opcode::SystemFindnext);
        pkt.push_u8(handle.handle);
        let mut recv = self.send_recv(&pkt).await?;
        let handle = recv.read_u8()?;
        let name = recv.read_filename()?;
        let len = recv.read_u32()?;
        Ok(FindFileHandle { handle, name, len })
    }

    /// Souce code just says `For internal use only`
    pub async fn file_open_read_linear(
        &self,
        name: &str,
        len: u32,
    ) -> Result<FileHandle> {
        let mut pkt = Packet::new(Opcode::SystemOpenreadlinear);
        pkt.push_filename(name)?;
        pkt.push_u32(len);
        let mut recv = self.send_recv(&pkt).await?;
        let handle = recv.read_u8()?;
        Ok(FileHandle { handle, len })
    }

    /// Souce code just says `For internal use only`
    pub async fn file_open_write_linear(
        &self,
        name: &str,
        len: u32,
    ) -> Result<FileHandle> {
        let mut pkt = Packet::new(Opcode::SystemOpenwritelinear);
        pkt.push_filename(name)?;
        pkt.push_u32(len);
        let mut recv = self.send_recv(&pkt).await?;
        let handle = recv.read_u8()?;
        Ok(FileHandle { handle, len })
    }

    /// Search for a module matching the specified pattern and return a
    /// handle to the search state
    pub async fn module_find_first(
        &self,
        pattern: &str,
    ) -> Result<ModuleHandle> {
        let mut pkt = Packet::new(Opcode::SystemFindfirstmodule);
        pkt.push_filename(pattern)?;
        let mut recv = self.send_recv(&pkt).await?;
        let handle = recv.read_u8()?;
        let name = recv.read_filename()?;
        let id = recv.read_u32()?;
        let len = recv.read_u32()?;
        let iomap_len = recv.read_u16()?;
        Ok(ModuleHandle {
            handle,
            name,
            id,
            len,
            iomap_len,
        })
    }

    /// Take a search handle and return the next match, or an error if
    /// there are no further matches
    pub async fn module_find_next(
        &self,
        handle: &ModuleHandle,
    ) -> Result<ModuleHandle> {
        let mut pkt = Packet::new(Opcode::SystemFindnextmodule);
        pkt.push_u8(handle.handle);
        let mut recv = self.send_recv(&pkt).await?;
        let handle = recv.read_u8()?;
        let name = recv.read_filename()?;
        let id = recv.read_u32()?;
        let len = recv.read_u32()?;
        let iomap_len = recv.read_u16()?;
        Ok(ModuleHandle {
            handle,
            name,
            id,
            len,
            iomap_len,
        })
    }

    /// Close the provided module handle
    pub async fn module_close(&self, handle: &ModuleHandle) -> Result<()> {
        let mut pkt = Packet::new(Opcode::SystemClosemodhandle);
        pkt.push_u8(handle.handle);
        self.send(&pkt, true).await
    }

    /// Read `count` bytes from the IO map belonging to the specified
    /// module at the given offset
    pub async fn read_io_map(
        &self,
        mod_id: u32,
        offset: u16,
        count: u16,
    ) -> Result<Vec<u8>> {
        let mut pkt = Packet::new(Opcode::SystemIomapread);
        pkt.push_u32(mod_id);
        pkt.push_u16(offset);
        pkt.push_u16(count);
        let mut recv = self.send_recv(&pkt).await?;
        let _mod_id = recv.read_u32()?;
        let len = recv.read_u16()?;
        let data = recv.read_slice(len as usize)?;
        Ok(data.to_vec())
    }

    /// Write the provided data into the IO map belongint to the
    /// specified module at the given offset
    pub async fn write_io_map(
        &self,
        mod_id: u32,
        offset: u16,
        data: &[u8],
    ) -> Result<u16> {
        let mut pkt = Packet::new(Opcode::SystemIomapwrite);
        pkt.push_u32(mod_id);
        pkt.push_u16(offset);
        pkt.push_u16(data.len().try_into()?);
        pkt.push_slice(data);
        let mut recv = self.send_recv(&pkt).await?;
        let _mod_id = recv.read_u32()?;
        recv.read_u16()
    }

    /// Enter firmware update mode - warning, this is not recoverable
    /// without loading new firmware (not currently supported by this
    /// crate)
    pub async fn boot(&self, sure: bool) -> Result<Vec<u8>> {
        if !sure {
            return Err(Error::Serialise(
                "Are you sure? This is not recoverable",
            ));
        }

        let mut pkt = Packet::new(Opcode::SystemBootcmd);
        pkt.push_slice(b"Let's dance: SAMBA\0");
        let mut recv = self.send_recv(&pkt).await?;
        Ok(recv.read_slice(4)?.to_vec())
    }

    /// Set the NXT brick's name to the provided value
    pub async fn set_brick_name(&self, name: &str) -> Result<()> {
        let mut pkt = Packet::new(Opcode::SystemSetbrickname);
        pkt.push_str(name, MAX_NAME_LEN)?;
        self.send(&pkt, true).await
    }

    /// Retrieve the Bluetooth address of the brick
    pub async fn get_bt_addr(&self) -> Result<[u8; 6]> {
        let pkt = Packet::new(Opcode::SystemBtgetaddr);
        let mut recv = self.send_recv(&pkt).await?;
        let addr = recv.read_slice(6)?;
        Ok(addr.try_into().unwrap())
    }

    /// Retrieve general device information from the brick:
    /// * Brick name
    /// * Bluetooth address
    /// * Signal strength of connected bricks
    /// * Available flash memory
    pub async fn get_device_info(&self) -> Result<DeviceInfo> {
        let pkt = Packet::new(Opcode::SystemDeviceinfo);
        let mut recv = self.send_recv(&pkt).await?;
        let name = recv.read_string(MAX_NAME_LEN)?;
        let bt_addr = [
            recv.read_u8()?,
            recv.read_u8()?,
            recv.read_u8()?,
            recv.read_u8()?,
            recv.read_u8()?,
            recv.read_u8()?,
        ];
        // unused
        recv.read_u8()?;
        let signal_strength = (
            recv.read_u8()?,
            recv.read_u8()?,
            recv.read_u8()?,
            recv.read_u8()?,
        );
        let flash = recv.read_u32()?;

        Ok(DeviceInfo {
            name,
            bt_addr,
            signal_strength,
            flash,
        })
    }

    /// Delete user flash storage
    pub async fn delete_user_flash(&self) -> Result<()> {
        let pkt = Packet::new(Opcode::SystemDeleteuserflash);
        self.send(&pkt, true).await
    }

    /// Poll the USB buffer for a command?
    pub async fn poll_command_length(&self, buf: BufType) -> Result<u8> {
        let mut pkt = Packet::new(Opcode::SystemPollcmdlen);
        pkt.push_u8(buf as u8);
        let mut recv = self.send_recv(&pkt).await?;
        let _buf_num = recv.read_u8()?;
        recv.read_u8()
    }

    /// Poll the USB buffer for a command?
    pub async fn poll_command(&self, buf: BufType, len: u8) -> Result<Vec<u8>> {
        let mut pkt = Packet::new(Opcode::SystemPollcmd);
        pkt.push_u8(buf as u8);
        pkt.push_u8(len);
        let mut recv = self.send_recv(&pkt).await?;
        let _buf = recv.read_u8()?;
        let len = recv.read_u8()?;
        let data = recv.read_slice(len as usize)?;
        Ok(data.to_vec())
    }

    /// Factory reset the bluetooth module
    pub async fn bluetooth_factory_reset(&self) -> Result<()> {
        let pkt = Packet::new(Opcode::SystemBtfactoryreset);
        self.send(&pkt, true).await
    }
}
