#![no_main]
#![no_std]

use rmk::macros::rmk_central;

#[path = "ws2812.rs"]
mod ws2812;

#[rmk_central]
mod keyboard_central {
    use embassy_nrf::gpio::{Level, Output, OutputDrive};
    use embassy_nrf::pwm::{Config, Prescaler, SequenceLoad, SequencePwm};
    use rmk::controller::PollingController;

    use crate::ws2812::{Role, Ws2812Indicator, PWM_TOP};

    #[controller(poll)]
    fn rgb() -> Ws2812Indicator {
        let mut config = Config::default();
        config.prescaler = Prescaler::Div1;
        config.max_duty = PWM_TOP;
        config.sequence_load = SequenceLoad::Common;

        let pwm = SequencePwm::new_1ch(p.PWM0, p.P0_24, config).unwrap();
        let ext = Output::new(p.P0_13, Level::Low, OutputDrive::Standard);

        Ws2812Indicator::new(pwm, ext, None, Role::Central)
    }
}
