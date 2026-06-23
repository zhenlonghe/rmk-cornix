use embassy_nrf::gpio::Output;
use embassy_nrf::pwm::{SequenceConfig, SequencePwm, SingleSequenceMode, SingleSequencer};
use embassy_nrf::saadc::Saadc;
use embassy_time::{Duration, Timer};
use rmk::ble::BleState;
use rmk::channel::{ControllerSub, CONTROLLER_CHANNEL};
use rmk::controller::{Controller, PollingController};
use rmk::event::ControllerEvent;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Central,
    Peripheral,
}

const BREATH_FRAMES: u32 = 60;
const BLINK_PERIOD: u32 = 30;
const CONNECT_SHOW_FRAMES: u32 = 75;
const PEER_SHOW_FRAMES: u32 = 90;
const FULL_SHOW_FRAMES: u32 = 90;
const LEVEL: u8 = 0x10;
const BREATH_PEAK: u8 = 0x20;
const BATTERY_LOW: u8 = 20;
const BATTERY_FULL: u8 = 95;
const BATTERY_SAMPLE_FRAMES: u32 = 900;
const ADC_DIVIDER_MEASURED: i32 = 2000;
const ADC_DIVIDER_TOTAL: i32 = 2806;

pub const PWM_TOP: u16 = 20;
const W0: u16 = 0x8000 | 6;
const W1: u16 = 0x8000 | 13;
const WRESET: u16 = 0x8000;
const SEQ_BITS: usize = 2 * 3 * 8;
const SEQ_RESET: usize = 40;
const SEQ_LEN: usize = SEQ_BITS + SEQ_RESET;

const fn breath_table() -> [u8; BREATH_FRAMES as usize] {
    let mut table = [0u8; BREATH_FRAMES as usize];
    let half = BREATH_FRAMES / 2;
    let mut i = 0u32;
    while i < BREATH_FRAMES {
        let up = if i <= half { i } else { BREATH_FRAMES - i };
        table[i as usize] = ((up * BREATH_PEAK as u32) / half) as u8;
        i += 1;
    }
    table
}

static BREATH: [u8; BREATH_FRAMES as usize] = breath_table();

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Grb {
    g: u8,
    r: u8,
    b: u8,
}

const OFF: Grb = Grb { g: 0, r: 0, b: 0 };
const RED: Grb = Grb {
    g: 0,
    r: LEVEL,
    b: 0,
};
const GREEN: Grb = Grb {
    g: LEVEL,
    r: 0,
    b: 0,
};
const BLUE: Grb = Grb {
    g: 0,
    r: 0,
    b: LEVEL,
};

pub struct Ws2812Indicator {
    pwm: SequencePwm<'static>,
    ext_power: Output<'static>,
    battery_adc: Option<Saadc<'static, 1>>,
    role: Role,
    sub: ControllerSub,

    battery: Option<u8>,
    charging: bool,
    ble_profile: u8,
    ble_connected: bool,
    ble_advertising: bool,
    peer_connected: bool,
    sleeping: bool,

    tick: u32,
    ble_frame: u32,
    peer_frame: u32,
    charge_frame: u32,
    battery_sample_frame: u32,
    rail_on: bool,
    last: Option<(Grb, Grb)>,
}

impl Ws2812Indicator {
    pub fn new(
        pwm: SequencePwm<'static>,
        mut ext_power: Output<'static>,
        battery_adc: Option<Saadc<'static, 1>>,
        role: Role,
    ) -> Self {
        ext_power.set_low();
        Self {
            pwm,
            ext_power,
            battery_adc,
            role,
            sub: match CONTROLLER_CHANNEL.subscriber() {
                Ok(sub) => sub,
                Err(_) => panic!("controller subscriber unavailable"),
            },
            battery: None,
            charging: false,
            ble_profile: 0,
            ble_connected: false,
            ble_advertising: false,
            peer_connected: false,
            sleeping: false,
            tick: 0,
            ble_frame: 0,
            peer_frame: 0,
            charge_frame: 0,
            battery_sample_frame: 0,
            rail_on: false,
            last: None,
        }
    }

    fn set_charging(&mut self, charging: bool) {
        if charging != self.charging {
            self.charging = charging;
            self.charge_frame = 0;
        }
    }

    fn profile_color(&self) -> Grb {
        match self.ble_profile {
            0 => RED,
            1 => GREEN,
            2 => BLUE,
            _ => BLUE,
        }
    }

    fn double_blink_on(&self) -> bool {
        let phase = self.tick % BLINK_PERIOD;
        phase < 6 || (12..18).contains(&phase)
    }

    fn breath_color(&self, color: Grb) -> Grb {
        let level = BREATH[(self.tick % BREATH_FRAMES) as usize];
        Grb {
            g: if color.g > 0 { level } else { 0 },
            r: if color.r > 0 { level } else { 0 },
            b: if color.b > 0 { level } else { 0 },
        }
    }

    fn battery_at_least(&self, threshold: u8) -> bool {
        matches!(self.battery, Some(level) if level >= threshold)
    }

    fn battery_at_most(&self, threshold: u8) -> bool {
        matches!(self.battery, Some(level) if level <= threshold)
    }

    fn inner_color(&self) -> Grb {
        if self.charging {
            if self.battery_at_least(BATTERY_FULL) {
                return if self.charge_frame < FULL_SHOW_FRAMES {
                    GREEN
                } else {
                    OFF
                };
            }
            return self.breath_color(GREEN);
        }

        if self.role == Role::Central {
            if !self.peer_connected {
                return self.breath_color(BLUE);
            }
            if self.peer_frame < PEER_SHOW_FRAMES {
                return BLUE;
            }
        }

        if self.battery_at_most(BATTERY_LOW) {
            return if self.double_blink_on() { RED } else { OFF };
        }

        OFF
    }

    fn outer_color(&self) -> Grb {
        match self.role {
            Role::Central => {
                if self.ble_connected {
                    if self.ble_frame < CONNECT_SHOW_FRAMES {
                        self.profile_color()
                    } else {
                        OFF
                    }
                } else if self.ble_advertising {
                    self.breath_color(self.profile_color())
                } else {
                    OFF
                }
            }
            Role::Peripheral => {
                if self.peer_connected {
                    if self.peer_frame < PEER_SHOW_FRAMES {
                        BLUE
                    } else {
                        OFF
                    }
                } else {
                    self.breath_color(BLUE)
                }
            }
        }
    }

    fn battery_percent_from_adc(val: i16) -> u8 {
        let val = val as i32;
        let full = 4755 * ADC_DIVIDER_MEASURED / ADC_DIVIDER_TOTAL;
        let empty = 4055 * ADC_DIVIDER_MEASURED / ADC_DIVIDER_TOTAL;

        if val > full {
            100
        } else if val < empty {
            0
        } else {
            ((val * ADC_DIVIDER_TOTAL / ADC_DIVIDER_MEASURED - 4055) / 7) as u8
        }
    }

    async fn sample_battery_if_due(&mut self) {
        if self.battery_sample_frame != 0 {
            return;
        }

        let was_full = self.battery_at_least(BATTERY_FULL);
        let Some(battery_adc) = self.battery_adc.as_mut() else {
            return;
        };

        let mut buf = [0i16; 1];
        battery_adc.sample(&mut buf).await;

        let level = Self::battery_percent_from_adc(buf[0]);
        self.battery = Some(level);
        if self.charging && level >= BATTERY_FULL && !was_full {
            self.charge_frame = 0;
        }
    }

    fn encode(buf: &mut [u16; SEQ_LEN], inner: Grb, outer: Grb) {
        let bytes = [inner.g, inner.r, inner.b, outer.g, outer.r, outer.b];
        let mut k = 0;
        for byte in bytes {
            let mut value = byte;
            for _ in 0..8 {
                buf[k] = if value & 0x80 != 0 { W1 } else { W0 };
                k += 1;
                value <<= 1;
            }
        }
        while k < SEQ_LEN {
            buf[k] = WRESET;
            k += 1;
        }
    }

    async fn render(&mut self, inner: Grb, outer: Grb) {
        if self.last == Some((inner, outer)) {
            return;
        }
        self.last = Some((inner, outer));

        let any_on = inner != OFF || outer != OFF;
        if any_on && !self.rail_on {
            self.ext_power.set_high();
            Timer::after(Duration::from_millis(5)).await;
            self.rail_on = true;
        }

        let mut buf = [WRESET; SEQ_LEN];
        Self::encode(&mut buf, inner, outer);
        {
            let seq = SingleSequencer::new(&mut self.pwm, &buf, SequenceConfig::default());
            if seq.start(SingleSequenceMode::Times(1)).is_ok() {
                Timer::after(Duration::from_millis(1)).await;
            }
        }

        if !any_on {
            self.ext_power.set_low();
            self.rail_on = false;
        }
    }
}

impl Controller for Ws2812Indicator {
    type Event = ControllerEvent;

    async fn process_event(&mut self, event: Self::Event) {
        match event {
            ControllerEvent::Battery(level) => {
                let was_full = self.battery_at_least(BATTERY_FULL);
                self.battery = Some(level);
                if self.charging && level >= BATTERY_FULL && !was_full {
                    self.charge_frame = 0;
                }
            }
            ControllerEvent::ChargingState(charging) => self.set_charging(charging),
            ControllerEvent::SplitPeripheral(_, connected) if self.role == Role::Central => {
                if connected != self.peer_connected {
                    self.peer_connected = connected;
                    self.peer_frame = 0;
                }
            }
            ControllerEvent::SplitCentral(connected) if self.role == Role::Peripheral => {
                if connected != self.peer_connected {
                    self.peer_connected = connected;
                    self.peer_frame = 0;
                }
            }
            ControllerEvent::BleState(profile, state) if self.role == Role::Central => {
                let connected = matches!(state, BleState::Connected);
                let advertising = matches!(state, BleState::Advertising);
                if (connected && !self.ble_connected)
                    || (advertising && !self.ble_advertising)
                    || profile != self.ble_profile
                {
                    self.ble_frame = 0;
                }
                self.ble_profile = profile;
                self.ble_connected = connected;
                self.ble_advertising = advertising;
            }
            ControllerEvent::BleProfile(profile) if self.role == Role::Central => {
                if profile != self.ble_profile {
                    self.ble_profile = profile;
                    self.ble_frame = 0;
                }
            }
            ControllerEvent::KeyboardIndicator(_) if self.role == Role::Central => {
                if !self.ble_connected {
                    self.ble_connected = true;
                    self.ble_frame = 0;
                }
            }
            ControllerEvent::Sleep(sleeping) => {
                self.sleeping = sleeping;
            }
            _ => {}
        }
    }

    async fn next_message(&mut self) -> Self::Event {
        self.sub.next_message_pure().await
    }
}

impl PollingController for Ws2812Indicator {
    const INTERVAL: Duration = Duration::from_millis(33);

    async fn update(&mut self) {
        if self.sleeping {
            self.render(OFF, OFF).await;
            return;
        }

        let usb_present = embassy_nrf::pac::POWER.usbregstatus().read().vbusdetect();
        self.set_charging(usb_present);
        self.sample_battery_if_due().await;

        let inner = self.inner_color();
        let outer = self.outer_color();
        self.render(inner, outer).await;
        self.tick = self.tick.wrapping_add(1);
        self.ble_frame = self.ble_frame.wrapping_add(1);
        self.peer_frame = self.peer_frame.wrapping_add(1);
        self.charge_frame = self.charge_frame.wrapping_add(1);
        self.battery_sample_frame = (self.battery_sample_frame + 1) % BATTERY_SAMPLE_FRAMES;
    }
}
