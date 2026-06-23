#![no_main]
#![no_std]

use rmk::macros::rmk_peripheral;

#[path = "ws2812.rs"]
mod ws2812;

#[rmk_peripheral(id = 0)]
mod keyboard_peripheral {
    use embassy_nrf::gpio::{Level, Output, OutputDrive};
    use embassy_nrf::pwm::{Config, Prescaler, SequenceLoad, SequencePwm};
    use embassy_nrf::saadc::{self, Input as _, Saadc};
    use rmk::controller::PollingController;

    use crate::ws2812::{Role, Ws2812Indicator, PWM_TOP};

    #[controller(poll)]
    fn rgb() -> Ws2812Indicator {
        embassy_nrf::bind_interrupts!(struct SaadcIrqs {
            SAADC => embassy_nrf::saadc::InterruptHandler;
        });

        let mut config = Config::default();
        config.prescaler = Prescaler::Div1;
        config.max_duty = PWM_TOP;
        config.sequence_load = SequenceLoad::Common;

        let pwm = SequencePwm::new_1ch(p.PWM0, p.P0_13, config).unwrap();
        let ext = Output::new(p.P0_24, Level::Low, OutputDrive::Standard);
        let adc_config = saadc::Config::default();
        embassy_nrf::interrupt::SAADC.set_priority(embassy_nrf::interrupt::Priority::P3);
        let battery_adc = Saadc::new(
            p.SAADC,
            SaadcIrqs,
            adc_config,
            [saadc::ChannelConfig::single_ended(p.P0_05.degrade_saadc())],
        );
        battery_adc.calibrate().await;

        Ws2812Indicator::new(pwm, ext, Some(battery_adc), Role::Peripheral)
    }
}
