use embassy_nrf::gpio::Output;
use embassy_nrf::pwm::{SequenceConfig, SequencePwm, SingleSequenceMode, SingleSequencer};
use embassy_nrf::saadc::Saadc;
use embassy_time::{Duration, Instant, Timer};
use rmk::ble::BleState;
use rmk::channel::{ControllerSub, CONTROLLER_CHANNEL};
use rmk::controller::{Controller, PollingController};
use rmk::embassy_futures::select::{select, Either};
use rmk::event::ControllerEvent;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Central,
    Peripheral,
}

// Polling cadence: fast while something is animating, slow when the LEDs are
// idle. The slow tick only exists to notice USB plug/unplug and to trigger the
// periodic battery sample, so it can be coarse to keep the radio asleep longer.
const ANIM_INTERVAL: Duration = Duration::from_millis(33);
const IDLE_INTERVAL: Duration = Duration::from_millis(1000);

// Time-based status windows (decoupled from the polling cadence).
const NOTICE: Duration = Duration::from_secs(3);
const ACTIVITY: Duration = Duration::from_secs(60);
const LOW_ALERT: Duration = Duration::from_secs(5);
const LOW_QUIET: Duration = Duration::from_secs(5 * 60);
const BATTERY_SAMPLE: Duration = Duration::from_secs(30);

// Per-frame easing step toward the target color (~200 ms full fade at 30 fps).
const FADE_STEP: u8 = 3;

const BREATH_STEPS: usize = 64;
const BREATH_PERIOD_MS: u64 = 3000;
const LOW_BLINK_PERIOD_MS: u64 = 1200;

const BATTERY_LOW: u8 = 20;
const BATTERY_FULL: u8 = 95;
const ADC_DIVIDER_MEASURED: i32 = 2000;
const ADC_DIVIDER_TOTAL: i32 = 2806;

pub const PWM_TOP: u16 = 20;
const W0: u16 = 0x8000 | 6;
const W1: u16 = 0x8000 | 13;
const WRESET: u16 = 0x8000;
const SEQ_BITS: usize = 2 * 3 * 8;
const SEQ_RESET: usize = 40;
const SEQ_LEN: usize = SEQ_BITS + SEQ_RESET;

/// Gamma-corrected breathing curve in `0..=255`, applied multiplicatively to a
/// color so the hue is preserved while the perceived brightness eases smoothly.
/// A linear ramp looks like it "jumps" bright then lingers dim; squaring it
/// (gamma ~2) matches the eye's logarithmic response for a silky breath.
const fn breath_lut() -> [u8; BREATH_STEPS] {
    let mut table = [0u8; BREATH_STEPS];
    let half = (BREATH_STEPS / 2) as u32;
    let mut i = 0usize;
    while i < BREATH_STEPS {
        let up = if (i as u32) < half {
            i as u32
        } else {
            BREATH_STEPS as u32 - i as u32
        };
        let lin = up * 1000 / half; // 0..=1000 triangle
        let gamma = lin * lin / 1000; // gamma 2
        table[i] = (gamma * 255 / 1000) as u8;
        i += 1;
    }
    table
}

static BREATH: [u8; BREATH_STEPS] = breath_lut();

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Grb {
    g: u8,
    r: u8,
    b: u8,
}

#[derive(Clone, Copy)]
enum LightEffect {
    Off,
    Solid(Grb),
    Breath(Grb),
    LowBattery,
}

impl LightEffect {
    fn is_animated(self) -> bool {
        matches!(self, Self::Breath(_) | Self::LowBattery)
    }
}

const OFF: Grb = Grb { g: 0, r: 0, b: 0 };
// Channel values are perceptually balanced (green reads brighter than red/blue
// at equal drive) and kept low for power. Hues stay distinct at low brightness.
const RED: Grb = Grb { g: 0, r: 18, b: 0 };
const GREEN: Grb = Grb { g: 12, r: 0, b: 0 };
const BLUE: Grb = Grb { g: 0, r: 0, b: 18 };
const MAGENTA: Grb = Grb { g: 0, r: 14, b: 16 };
const CYAN: Grb = Grb { g: 10, r: 0, b: 14 };

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
    caps_lock: bool,
    sleeping: bool,

    ble_since: Instant,
    peer_since: Instant,
    charge_since: Instant,
    low_alert_until: Instant,
    low_next_alert: Instant,
    last_battery_sample: Option<Instant>,

    cur_inner: Grb,
    cur_outer: Grb,
    tgt_inner: Grb,
    tgt_outer: Grb,
    inner_active: bool,
    outer_active: bool,
    pending: bool,
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
        let now = Instant::now();
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
            caps_lock: false,
            sleeping: false,
            ble_since: now,
            peer_since: now,
            charge_since: now,
            low_alert_until: now,
            low_next_alert: now,
            last_battery_sample: None,
            cur_inner: OFF,
            cur_outer: OFF,
            tgt_inner: OFF,
            tgt_outer: OFF,
            inner_active: false,
            outer_active: false,
            pending: true,
            rail_on: false,
            last: None,
        }
    }

    fn set_charging(&mut self, charging: bool) {
        if charging != self.charging {
            self.charging = charging;
            self.charge_since = Instant::now();
            self.pending = true;
        }
    }

    fn set_battery_level(&mut self, level: u8) {
        let was_full = self.battery_at_least(BATTERY_FULL);
        self.battery = Some(level);
        if self.charging && level >= BATTERY_FULL && !was_full {
            self.charge_since = Instant::now();
        }
        self.pending = true;
    }

    fn set_peer_connected(&mut self, connected: bool) {
        if connected != self.peer_connected {
            self.peer_since = Instant::now();
            self.peer_connected = connected;
            self.pending = true;
        }
    }

    fn set_caps_lock(&mut self, enabled: bool) {
        if enabled != self.caps_lock {
            self.caps_lock = enabled;
            self.pending = true;
        }
    }

    fn set_ble_state(&mut self, profile: u8, state: BleState) {
        let connected = matches!(state, BleState::Connected);
        let advertising = matches!(state, BleState::Advertising);
        let changed = connected != self.ble_connected
            || advertising != self.ble_advertising
            || profile != self.ble_profile;

        if changed {
            self.ble_since = Instant::now();
            self.pending = true;
        }

        self.ble_profile = profile;
        self.ble_connected = connected;
        self.ble_advertising = advertising;
    }

    fn set_ble_profile(&mut self, profile: u8) {
        if profile != self.ble_profile {
            self.ble_profile = profile;
            self.ble_since = Instant::now();
            self.pending = true;
        }
    }

    fn set_sleeping(&mut self, sleeping: bool) {
        if self.sleeping != sleeping {
            if !sleeping {
                // Re-show the connection/peer notices when waking up.
                self.ble_since = Instant::now();
                self.peer_since = Instant::now();
            }
            self.sleeping = sleeping;
            self.pending = true;
        }
    }

    fn profile_color(&self) -> Grb {
        match self.ble_profile {
            0 => RED,
            1 => GREEN,
            2 => BLUE,
            3 => MAGENTA,
            _ => CYAN,
        }
    }

    fn breath_level(&self) -> u8 {
        let ms = Instant::now().as_millis() % BREATH_PERIOD_MS;
        let idx = (ms as usize * BREATH_STEPS / BREATH_PERIOD_MS as usize) % BREATH_STEPS;
        BREATH[idx]
    }

    fn breath_color(&self, color: Grb) -> Grb {
        let level = self.breath_level() as u16;
        Grb {
            g: (color.g as u16 * level / 255) as u8,
            r: (color.r as u16 * level / 255) as u8,
            b: (color.b as u16 * level / 255) as u8,
        }
    }

    fn low_blink_on(&self) -> bool {
        let ms = Instant::now().as_millis() % LOW_BLINK_PERIOD_MS;
        ms < 200 || (400..600).contains(&ms)
    }

    fn effect_color(&self, effect: LightEffect) -> Grb {
        match effect {
            LightEffect::Off => OFF,
            LightEffect::Solid(color) => color,
            LightEffect::Breath(color) => self.breath_color(color),
            LightEffect::LowBattery => {
                if self.low_blink_on() {
                    RED
                } else {
                    OFF
                }
            }
        }
    }

    fn battery_at_least(&self, threshold: u8) -> bool {
        matches!(self.battery, Some(level) if level >= threshold)
    }

    fn battery_at_most(&self, threshold: u8) -> bool {
        matches!(self.battery, Some(level) if level <= threshold)
    }

    fn low_blinking(&self) -> bool {
        self.battery_at_most(BATTERY_LOW) && Instant::now() < self.low_alert_until
    }

    fn inner_effect(&self) -> LightEffect {
        if self.charging {
            if self.battery_at_least(BATTERY_FULL) {
                return if self.charge_since.elapsed() < NOTICE {
                    LightEffect::Solid(GREEN)
                } else {
                    LightEffect::Off
                };
            }
            return LightEffect::Breath(GREEN);
        }

        if self.low_blinking() {
            return LightEffect::LowBattery;
        }

        if self.role == Role::Central {
            if !self.peer_connected {
                return if self.peer_since.elapsed() < ACTIVITY {
                    LightEffect::Breath(BLUE)
                } else {
                    LightEffect::Off
                };
            }
            if self.peer_since.elapsed() < NOTICE {
                return LightEffect::Solid(BLUE);
            }
        }

        LightEffect::Off
    }

    fn outer_effect(&self) -> LightEffect {
        match self.role {
            Role::Central => {
                if self.ble_connected && self.ble_since.elapsed() < NOTICE {
                    LightEffect::Solid(self.profile_color())
                } else if self.ble_advertising && self.ble_since.elapsed() < ACTIVITY {
                    LightEffect::Breath(self.profile_color())
                } else if self.caps_lock {
                    LightEffect::Solid(CYAN)
                } else {
                    LightEffect::Off
                }
            }
            Role::Peripheral => {
                if self.peer_connected {
                    if self.peer_since.elapsed() < NOTICE {
                        LightEffect::Solid(BLUE)
                    } else {
                        LightEffect::Off
                    }
                } else if self.peer_since.elapsed() < ACTIVITY {
                    LightEffect::Breath(BLUE)
                } else {
                    LightEffect::Off
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

    async fn sample_battery_if_due(&mut self, now: Instant) {
        let due = match self.last_battery_sample {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= BATTERY_SAMPLE,
        };
        if !due {
            return;
        }
        self.last_battery_sample = Some(now);

        let Some(battery_adc) = self.battery_adc.as_mut() else {
            return;
        };

        let mut buf = [0i16; 1];
        battery_adc.sample(&mut buf).await;
        self.set_battery_level(Self::battery_percent_from_adc(buf[0]));
    }

    /// Schedule the low-battery alert: blink for `LOW_ALERT`, stay quiet for
    /// `LOW_QUIET`, then repeat while the battery remains low.
    fn refresh_low_battery(&mut self, now: Instant) {
        if !self.battery_at_most(BATTERY_LOW) {
            // Not low: keep the schedule armed at `now` so a fresh drop back into
            // low battery alerts immediately, instead of being deferred by a
            // stale quiet window left over from a previous low-battery episode.
            self.low_alert_until = now;
            self.low_next_alert = now;
            return;
        }
        if now >= self.low_next_alert {
            self.low_alert_until = now + LOW_ALERT;
            self.low_next_alert = now + LOW_ALERT + LOW_QUIET;
        }
    }

    fn is_animating(&self) -> bool {
        // Animated effects must keep the fast cadence even when their
        // instantaneous color is momentarily OFF, e.g. the breath trough or
        // the dark phase of the low-battery blink. Solid effects can drop to
        // the idle cadence once their fade has settled.
        self.inner_active
            || self.outer_active
            || self.cur_inner != self.tgt_inner
            || self.cur_outer != self.tgt_outer
    }

    fn approach_channel(cur: u8, target: u8, step: u8) -> u8 {
        if cur < target {
            cur.saturating_add(step).min(target)
        } else if cur > target {
            cur.saturating_sub(step).max(target)
        } else {
            cur
        }
    }

    fn approach(cur: Grb, target: Grb, step: u8) -> Grb {
        Grb {
            g: Self::approach_channel(cur.g, target.g, step),
            r: Self::approach_channel(cur.r, target.r, step),
            b: Self::approach_channel(cur.b, target.b, step),
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

        // Only cut the rail once the fade-out has fully settled to off, so the
        // last visible frame is black rather than an abrupt power cut.
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
            ControllerEvent::Battery(level) => self.set_battery_level(level),
            ControllerEvent::ChargingState(charging) => self.set_charging(charging),
            ControllerEvent::SplitPeripheral(_, connected) if self.role == Role::Central => {
                self.set_peer_connected(connected);
            }
            ControllerEvent::SplitCentral(connected) if self.role == Role::Peripheral => {
                self.set_peer_connected(connected);
            }
            ControllerEvent::BleState(profile, state) if self.role == Role::Central => {
                self.set_ble_state(profile, state);
            }
            ControllerEvent::BleProfile(profile) if self.role == Role::Central => {
                self.set_ble_profile(profile);
            }
            ControllerEvent::KeyboardIndicator(indicator) if self.role == Role::Central => {
                self.set_caps_lock(indicator.caps_lock());
            }
            ControllerEvent::Sleep(sleeping) => self.set_sleeping(sleeping),
            _ => {}
        }
    }

    async fn next_message(&mut self) -> Self::Event {
        self.sub.next_message_pure().await
    }
}

impl PollingController for Ws2812Indicator {
    // Required by the trait; the adaptive `polling_loop` below picks the actual
    // cadence per cycle, so this is just the fast (animation) interval.
    const INTERVAL: Duration = ANIM_INTERVAL;

    async fn update(&mut self) {
        let now = Instant::now();

        if self.sleeping {
            self.inner_active = false;
            self.outer_active = false;
            self.tgt_inner = OFF;
            self.tgt_outer = OFF;
        } else {
            let usb_present = embassy_nrf::pac::POWER.usbregstatus().read().vbusdetect();
            self.set_charging(usb_present);
            self.sample_battery_if_due(now).await;
            self.refresh_low_battery(now);
            let inner = self.inner_effect();
            let outer = self.outer_effect();
            self.inner_active = inner.is_animated();
            self.outer_active = outer.is_animated();
            self.tgt_inner = self.effect_color(inner);
            self.tgt_outer = self.effect_color(outer);
        }

        self.cur_inner = Self::approach(self.cur_inner, self.tgt_inner, FADE_STEP);
        self.cur_outer = Self::approach(self.cur_outer, self.tgt_outer, FADE_STEP);
        self.render(self.cur_inner, self.cur_outer).await;
        self.pending = false;
    }

    /// Adaptive loop: animate at ~30 fps while anything is lit or fading, and
    /// drop to a slow idle tick once the LEDs settle to off, which lets the
    /// radio sleep undisturbed and cuts idle wakeups roughly tenfold.
    ///
    /// Tracks an absolute deadline rather than restarting the wait on every
    /// event. Key presses are published to the controller channel, so during
    /// fast typing events arrive continuously; resetting the interval each time
    /// would push `update()` out indefinitely and starve the animation,
    /// fade-out and battery sampling.
    async fn polling_loop(&mut self) {
        let mut deadline = Instant::now();
        loop {
            let now = Instant::now();
            if now >= deadline {
                self.update().await;
                let interval = if self.is_animating() {
                    ANIM_INTERVAL
                } else {
                    IDLE_INTERVAL
                };
                deadline = Instant::now() + interval;
                continue;
            }

            match select(Timer::after(deadline - now), self.next_message()).await {
                Either::First(_) => {
                    self.update().await;
                    let interval = if self.is_animating() {
                        ANIM_INTERVAL
                    } else {
                        IDLE_INTERVAL
                    };
                    deadline = Instant::now() + interval;
                }
                Either::Second(event) => {
                    self.process_event(event).await;
                    // Reflect a state-changing event promptly by pulling the
                    // deadline in; never push it out (ignored events keep it).
                    if self.pending {
                        let soon = Instant::now() + ANIM_INTERVAL;
                        if soon < deadline {
                            deadline = soon;
                        }
                    }
                }
            }
        }
    }
}
