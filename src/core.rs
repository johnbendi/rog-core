// Return show-stopping errors, otherwise map error to a log level

use crate::{
    aura::{aura_brightness_bytes, BuiltInModeByte},
    config::Config,
    error::AuraError,
    virt_device::VirtKeys,
};
use gumdrop::Options;
use log::{debug, error, info, warn};
use rusb::DeviceHandle;
use std::error::Error;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::marker::{PhantomData, PhantomPinned};
use std::path::Path;
use std::process::Command;
use std::ptr::NonNull;
use std::str::FromStr;
use std::time::Duration;

static LED_INIT1: [u8; 2] = [0x5d, 0xb9];
static LED_INIT2: &str = "]ASUS Tech.Inc."; // ] == 0x5d
static LED_INIT3: [u8; 6] = [0x5d, 0x05, 0x20, 0x31, 0, 0x08];
static LED_INIT4: &str = "^ASUS Tech.Inc."; // ^ == 0x5e
static LED_INIT5: [u8; 6] = [0x5e, 0x05, 0x20, 0x31, 0, 0x08];

// Only these two packets must be 17 bytes
static LED_APPLY: [u8; 17] = [0x5d, 0xb4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
static LED_SET: [u8; 17] = [0x5d, 0xb5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

static FAN_TYPE_1_PATH: &str = "/sys/devices/platform/asus-nb-wmi/throttle_thermal_policy";
static FAN_TYPE_2_PATH: &str = "/sys/devices/platform/asus-nb-wmi/fan_boost_mode";

/// ROG device controller
///
/// For the GX502GW the LED setup sequence looks like:
///
/// -` LED_INIT1`
/// - `LED_INIT3`
/// - `LED_INIT4`
/// - `LED_INIT2`
/// - `LED_INIT4`
pub(crate) struct RogCore {
    handle: DeviceHandle<rusb::GlobalContext>,
    virt_keys: VirtKeys,
    _pin: PhantomPinned,
}

impl RogCore {
    pub(crate) fn new(vendor: u16, product: u16, led_endpoint: u8) -> Result<RogCore, AuraError> {
        let mut dev_handle = RogCore::get_device(vendor, product)?;
        dev_handle.set_active_configuration(0).unwrap_or(());

        let dev_config = dev_handle.device().config_descriptor(0).unwrap();
        // Interface with outputs
        let mut interface = 0;
        for iface in dev_config.interfaces() {
            for desc in iface.descriptors() {
                for endpoint in desc.endpoint_descriptors() {
                    if endpoint.address() == led_endpoint {
                        info!("INTERVAL: {:?}", endpoint.interval());
                        info!("MAX_PKT_SIZE: {:?}", endpoint.max_packet_size());
                        info!("SYNC: {:?}", endpoint.sync_type());
                        info!("TRANSFER_TYPE: {:?}", endpoint.transfer_type());
                        info!("ENDPOINT: {:X?}", endpoint.address());
                        interface = desc.interface_number();
                        break;
                    }
                }
            }
        }

        dev_handle.set_auto_detach_kernel_driver(true).unwrap();
        dev_handle
            .claim_interface(interface)
            .map_err(AuraError::UsbError)?;

        Ok(RogCore {
            handle: dev_handle,
            virt_keys: VirtKeys::new(),
            _pin: PhantomPinned,
        })
    }

    pub(crate) async fn reload(&mut self, config: &mut Config) -> Result<(), Box<dyn Error>> {
        // let mode_curr = self.config.current_mode[3];
        // let mode = self
        //     .config
        //     .builtin_modes
        //     .get_field_from(BuiltInModeByte::from(mode_curr).into())
        //     .unwrap()
        //     .to_owned();
        // self.aura_write_messages(&[&mode])?;

        let path = if Path::new(FAN_TYPE_1_PATH).exists() {
            FAN_TYPE_1_PATH
        } else if Path::new(FAN_TYPE_2_PATH).exists() {
            FAN_TYPE_2_PATH
        } else {
            return Ok(());
        };

        let mut file = OpenOptions::new().write(true).open(path)?;
        file.write_all(format!("{:?}\n", config.fan_mode).as_bytes())?;
        self.set_pstate_for_fan_mode(FanLevel::from(config.fan_mode))?;
        info!("Reloaded last saved settings");
        Ok(())
    }

    pub(crate) fn virt_keys(&mut self) -> &mut VirtKeys {
        &mut self.virt_keys
    }

    fn get_device(
        vendor: u16,
        product: u16,
    ) -> Result<DeviceHandle<rusb::GlobalContext>, AuraError> {
        for device in rusb::devices().unwrap().iter() {
            let device_desc = device.device_descriptor().unwrap();
            if device_desc.vendor_id() == vendor && device_desc.product_id() == product {
                return device.open().map_err(|err| AuraError::UsbError(err));
            }
        }
        Err(AuraError::UsbError(rusb::Error::NoDevice))
    }

    pub(crate) fn fan_mode_step(&mut self, config: &mut Config) -> Result<(), Box<dyn Error>> {
        let path = if Path::new(FAN_TYPE_1_PATH).exists() {
            FAN_TYPE_1_PATH
        } else if Path::new(FAN_TYPE_2_PATH).exists() {
            FAN_TYPE_2_PATH
        } else {
            return Ok(());
        };

        let mut fan_ctrl = OpenOptions::new().read(true).write(true).open(path)?;

        let mut buf = String::new();
        if let Ok(_) = fan_ctrl.read_to_string(&mut buf) {
            let mut n = u8::from_str_radix(&buf.trim_end(), 10)?;
            info!("Current fan mode: {:#?}", FanLevel::from(n));
            // wrap around the step number
            if n < 2 {
                n += 1;
            } else {
                n = 0;
            }
            info!("Fan mode stepped to: {:#?}", FanLevel::from(n));
            fan_ctrl.write_all(format!("{:?}\n", n).as_bytes())?;
            self.set_pstate_for_fan_mode(FanLevel::from(n))?;
            config.fan_mode = n;
            config.write();
        }
        Ok(())
    }

    fn set_pstate_for_fan_mode(&self, mode: FanLevel) -> Result<(), Box<dyn Error>> {
        // Set CPU pstate
        if let Ok(pstate) = intel_pstate::PState::new() {
            match mode {
                FanLevel::Normal => {
                    pstate.set_min_perf_pct(0)?;
                    pstate.set_max_perf_pct(100)?;
                    pstate.set_no_turbo(false)?;
                    info!("CPU pstate: normal");
                }
                FanLevel::Boost => {
                    pstate.set_min_perf_pct(50)?;
                    pstate.set_max_perf_pct(100)?;
                    pstate.set_no_turbo(false)?;
                    info!("CPU pstate: boost");
                }
                FanLevel::Silent => {
                    pstate.set_min_perf_pct(0)?;
                    pstate.set_max_perf_pct(70)?;
                    pstate.set_no_turbo(true)?;
                    info!("CPU pstate: silent, no-turbo");
                }
            }
        }
        Ok(())
    }

    /// A direct call to systemd to suspend the PC.
    ///
    /// This avoids desktop environments being required to handle it
    /// (which means it works while in a TTY also)
    pub(crate) fn suspend_with_systemd(&self) {
        std::process::Command::new("systemctl")
            .arg("suspend")
            .spawn()
            .map_or_else(|err| warn!("Failed to suspend: {}", err), |_| {});
    }

    /// A direct call to rfkill to suspend wireless devices.
    ///
    /// This avoids desktop environments being required to handle it (which
    /// means it works while in a TTY also)
    pub(crate) fn toggle_airplane_mode(&self) {
        match Command::new("rfkill").arg("list").output() {
            Ok(output) => {
                if output.status.success() {
                    if let Ok(out) = String::from_utf8(output.stdout) {
                        if out.contains(": yes") {
                            Command::new("rfkill")
                                .arg("unblock")
                                .arg("all")
                                .spawn()
                                .map_or_else(
                                    |err| warn!("Could not unblock rf devices: {}", err),
                                    |_| {},
                                );
                        } else {
                            Command::new("rfkill")
                                .arg("block")
                                .arg("all")
                                .spawn()
                                .map_or_else(
                                    |err| warn!("Could not block rf devices: {}", err),
                                    |_| {},
                                );
                        }
                    }
                } else {
                    warn!("Could not list rf devices");
                }
            }
            Err(err) => {
                warn!("Could not list rf devices: {}", err);
            }
        }
    }

    pub(crate) fn get_raw_device_handle(&mut self) -> NonNull<DeviceHandle<rusb::GlobalContext>> {
        // Breaking every damn lifetime guarantee rust gives us
        unsafe {
            NonNull::new_unchecked(&mut self.handle as *mut DeviceHandle<rusb::GlobalContext>)
        }
    }
}

/// Lifetime is tied to `DeviceHandle` from `RogCore`
pub(crate) struct KeyboardReader<'d, C: 'd>
where
    C: rusb::UsbContext,
{
    handle: NonNull<DeviceHandle<C>>,
    endpoint: u8,
    filter: Vec<u8>,
    _phantom: PhantomData<&'d DeviceHandle<C>>,
}

/// UNSAFE
unsafe impl<'d, C> Send for KeyboardReader<'d, C> where C: rusb::UsbContext {}
unsafe impl<'d, C> Sync for KeyboardReader<'d, C> where C: rusb::UsbContext {}

impl<'d, C> KeyboardReader<'d, C>
where
    C: rusb::UsbContext,
{
    pub fn new(device_handle: NonNull<DeviceHandle<C>>, key_endpoint: u8, filter: Vec<u8>) -> Self {
        KeyboardReader {
            handle: device_handle,
            endpoint: key_endpoint,
            filter,
            _phantom: PhantomData,
        }
    }

    /// Write the bytes read from the device interrupt to the buffer arg, and returns the
    /// count of bytes written
    ///
    /// `report_filter_bytes` is used to filter the data read from the interupt so
    /// only the relevant byte array is returned.
    pub(crate) async fn poll_keyboard(&self) -> Option<[u8; 32]> {
        let mut buf = [0u8; 32];
        match unsafe { self.handle.as_ref() }.read_interrupt(
            self.endpoint,
            &mut buf,
            Duration::from_millis(200),
        ) {
            Ok(_) => {
                if self.filter.contains(&buf[0])
                    && (buf[1] != 0 || buf[2] != 0 || buf[3] != 0 || buf[4] != 0)
                {
                    return Some(buf);
                }
            }
            Err(err) => match err {
                rusb::Error::Timeout => {}
                _ => error!("Failed to read keyboard interrupt: {:?}", err),
            },
        }
        None
    }
}

/// UNSAFE: Must live as long as RogCore
///
/// Because we're holding a pointer to something that *may* go out of scope while the
/// pointer is held. We're relying on access to struct to be behind a Mutex, and for behaviour
/// that may cause invalididated pointer to cause the program to panic rather than continue.
pub(crate) struct LedWriter<'d, C: 'd>
where
    C: rusb::UsbContext,
{
    handle: NonNull<DeviceHandle<C>>,
    led_endpoint: u8,
    initialised: bool,
    _phantom: PhantomData<&'d DeviceHandle<C>>,
}

/// UNSAFE
unsafe impl<'d, C> Send for LedWriter<'d, C> where C: rusb::UsbContext {}
unsafe impl<'d, C> Sync for LedWriter<'d, C> where C: rusb::UsbContext {}

impl<'d, C> LedWriter<'d, C>
where
    C: rusb::UsbContext,
{
    pub fn new(device_handle: NonNull<DeviceHandle<C>>, led_endpoint: u8) -> Self {
        LedWriter {
            handle: device_handle,
            led_endpoint,
            initialised: false,
            _phantom: PhantomData,
        }
    }

    async fn aura_write(&mut self, message: &[u8]) -> Result<(), AuraError> {
        match unsafe { self.handle.as_ref() }.write_interrupt(
            self.led_endpoint,
            message,
            Duration::from_millis(1),
        ) {
            Ok(_) => {}
            Err(err) => match err {
                rusb::Error::Timeout => {}
                _ => error!("Failed to read keyboard interrupt: {:?}", err),
            },
        }
        Ok(())
    }

    async fn aura_write_messages(&mut self, messages: &[&[u8]]) -> Result<(), AuraError> {
        if !self.initialised {
            self.aura_write(&LED_INIT1).await?;
            self.aura_write(LED_INIT2.as_bytes()).await?;
            self.aura_write(&LED_INIT3).await?;
            self.aura_write(LED_INIT4.as_bytes()).await?;
            self.aura_write(&LED_INIT5).await?;
            self.initialised = true;
        }

        for message in messages {
            self.aura_write(*message).await?;
            self.aura_write(&LED_SET).await?;
        }
        // Changes won't persist unless apply is set
        self.aura_write(&LED_APPLY).await?;
        Ok(())
    }

    /// Write an effect block
    ///
    /// `aura_effect_init` must be called any effect routine, and called only once.
    pub async fn async_write_effect(
        &self,
        endpoint: u8,
        effect: Vec<Vec<u8>>,
    ) -> Result<(), AuraError> {
        for row in effect.iter() {
            match unsafe { self.handle.as_ref() }.write_interrupt(
                endpoint,
                row,
                Duration::from_millis(1),
            ) {
                Ok(_) => {}
                Err(err) => match err {
                    rusb::Error::Timeout => {}
                    _ => error!("Failed to write LED interrupt: {:?}", err),
                },
            }
        }
        Ok(())
    }

    pub(crate) async fn aura_set_and_save(
        &mut self,
        supported_modes: &[BuiltInModeByte],
        bytes: &[u8],
        config: &mut Config,
    ) -> Result<(), AuraError> {
        let mode = BuiltInModeByte::from(bytes[3]);
        if bytes[1] == 0xbc {
            self.aura_write(bytes).await?;
            return Ok(());
        } else if supported_modes.contains(&mode) || bytes[1] == 0xba {
            let messages = [bytes];
            self.aura_write_messages(&messages).await?;
            config.set_field_from(bytes);
            config.write();
            return Ok(());
        }
        warn!("{:?} not supported", mode);
        Err(AuraError::NotSupported)
    }

    pub(crate) async fn aura_bright_inc(
        &mut self,
        supported_modes: &[BuiltInModeByte],
        max_bright: u8,
        config: &mut Config,
    ) -> Result<(), AuraError> {
        let mut bright = config.brightness;
        if bright < max_bright {
            bright += 1;
            config.brightness = bright;
            let bytes = aura_brightness_bytes(bright);
            self.aura_set_and_save(supported_modes, &bytes, config)
                .await?;
            info!("Increased LED brightness to {:#?}", bright);
        }
        Ok(())
    }

    pub(crate) async fn aura_bright_dec(
        &mut self,
        supported_modes: &[BuiltInModeByte],
        min_bright: u8,
        config: &mut Config,
    ) -> Result<(), AuraError> {
        let mut bright = config.brightness;
        if bright > min_bright {
            bright -= 1;
            config.brightness = bright;
            let bytes = aura_brightness_bytes(bright);
            self.aura_set_and_save(supported_modes, &bytes, config)
                .await?;
            info!("Decreased LED brightness to {:#?}", bright);
        }
        Ok(())
    }

    /// Select next Aura effect
    ///
    /// If the current effect is the last one then the effect selected wraps around to the first.
    pub(crate) async fn aura_mode_next(
        &mut self,
        supported_modes: &[BuiltInModeByte],
        config: &mut Config,
    ) -> Result<(), AuraError> {
        // TODO: different path for multi-zone (byte 2 controlled, non-zero)
        let mode_curr = config.current_mode[3];
        let idx = supported_modes.binary_search(&mode_curr.into()).unwrap();
        let idx_next = if idx < supported_modes.len() - 1 {
            idx + 1
        } else {
            0
        };
        let mode_next = config
            .builtin_modes
            .get_field_from(supported_modes[idx_next].into())
            .unwrap()
            .to_owned();
        self.aura_set_and_save(supported_modes, &mode_next, config)
            .await?;
        info!("Switched LED mode to {:#?}", supported_modes[idx_next]);
        Ok(())
    }

    /// Select previous Aura effect
    ///
    /// If the current effect is the first one then the effect selected wraps around to the last.
    pub(crate) async fn aura_mode_prev(
        &mut self,
        supported_modes: &[BuiltInModeByte],
        config: &mut Config,
    ) -> Result<(), AuraError> {
        // TODO: different path for multi-zone (byte 2 controlled, non-zero)
        let mode_curr = config.current_mode[3];
        let idx = supported_modes.binary_search(&mode_curr.into()).unwrap();
        let idx_next = if idx > 0 {
            idx - 1
        } else {
            supported_modes.len() - 1
        };
        let mode_next = config
            .builtin_modes
            .get_field_from(supported_modes[idx_next].into())
            .unwrap()
            .to_owned();
        self.aura_set_and_save(supported_modes, &mode_next, config)
            .await?;
        info!("Switched LED mode to {:#?}", supported_modes[idx_next]);
        Ok(())
    }
}

#[derive(Debug, Options)]
pub struct LedBrightness {
    level: u8,
}
impl LedBrightness {
    pub fn level(&self) -> u8 {
        self.level
    }
}
impl FromStr for LedBrightness {
    type Err = AuraError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.to_lowercase();
        match s.as_str() {
            "off" => Ok(LedBrightness { level: 0x00 }),
            "low" => Ok(LedBrightness { level: 0x01 }),
            "med" => Ok(LedBrightness { level: 0x02 }),
            "high" => Ok(LedBrightness { level: 0x03 }),
            _ => {
                println!("Missing required argument, must be one of:\noff,low,med,high\n");
                Err(AuraError::ParseBrightness)
            }
        }
    }
}

#[derive(Debug)]
enum FanLevel {
    Normal,
    Boost,
    Silent,
}

impl From<u8> for FanLevel {
    fn from(n: u8) -> Self {
        match n {
            0 => FanLevel::Normal,
            1 => FanLevel::Boost,
            2 => FanLevel::Silent,
            _ => FanLevel::Normal,
        }
    }
}

impl From<FanLevel> for u8 {
    fn from(n: FanLevel) -> Self {
        match n {
            FanLevel::Normal => 0,
            FanLevel::Boost => 1,
            FanLevel::Silent => 2,
        }
    }
}
