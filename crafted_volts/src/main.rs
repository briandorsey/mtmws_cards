#![no_std]
#![no_main]

use defmt::*;
use embassy_executor::Spawner;
use embassy_rp::adc;
use embassy_rp::bind_interrupts;
use embassy_rp::gpio::{self};
use embassy_rp::peripherals;
use embassy_rp::pwm;
use embassy_rp::pwm::SetDutyCycle;
use embassy_rp::spi;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::watch::Watch;
use embassy_time::Timer;

use gpio::{Level, Output};
use {defmt_rtt as _, panic_probe as _};

// high level notes...
// This is an attempt to learn how use all inputs & outputs of the Music Thing Modular Workshop System Computer via Rust
// The card maps knobs and the switch to manually set voltages.

// most inputs seem to be numbers from 0..4096 (12 bit), sometimes inverted from the thing they represent.
// most outputs seem to be numbers from 0..2048 (11 bit), sometimes inverted from the thing they represent.

// TODO: review all math, refactor and simplify, add clamp(), scale()
// TODO: smooth analog knob reads
// TODO: decide how to handle all unwraps properly
// TODO: review pwm frequencies
// future features
// TODO: implement audio input mixing / attenuation?
// TODO: implement CV input mixing / attenuation?
// TODO: implement pulse input mixing / attenuation?
// TODO: consider event based pulse updates: only change pulse outputs on switch change or pulse input edge detection (rather than on a loop)
// TODO: read and use calibration data from EEPROM
// TODO: read about defmt levels and overhead (can we leave logging statements in a release build? What are the effects?)

bind_interrupts!(struct Irqs {
    ADC_IRQ_FIFO => adc::InterruptHandler;
});

// single writer, multple reader
static WATCH_INPUT: Watch<CriticalSectionRawMutex, MuxState, 2> = Watch::new();

#[derive(Clone, Format)]
enum ZSwitch {
    On,
    Off,
    Momentary,
}

impl ZSwitch {
    fn default() -> ZSwitch {
        ZSwitch::Off
    }
}

// currently saving raw input values.
// TODO: decide on a normalization strategy (rp2040 doesn't have FP)
#[derive(Clone, Format)]
struct MuxState {
    main_knob: u16,
    x_knob: u16,
    y_knob: u16,
    zswitch: ZSwitch,
    cv1: u16,
    cv2: u16,
}

impl MuxState {
    fn default() -> Self {
        MuxState {
            main_knob: 2048,
            x_knob: 2048,
            y_knob: 2048,
            zswitch: ZSwitch::default(),
            // CV inputs are not inverted according to docs.  0V reads ~ 2030
            // NOTE: I get inverted data, and ~2060 as 0v
            cv1: 2060,
            cv2: 2060,
        }
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("Starting main()");
    let p = embassy_rp::init(Default::default());
    let mut led5 = Output::new(p.PIN_14, Level::Low);
    let mut led6 = Output::new(p.PIN_15, Level::Low);

    // pulse outputs are inverted
    let mut pulse_1_raw_out = Output::new(p.PIN_8, Level::High);
    let mut pulse_2_raw_out = Output::new(p.PIN_9, Level::High);

    // Set mux to read switch Z
    let mut muxlogic_a = Output::new(p.PIN_24, Level::Low);
    let mut muxlogic_b = Output::new(p.PIN_25, Level::Low);

    let mut mux_adc = adc::Adc::new(p.ADC, Irqs, adc::Config::default());
    let mut mux_io_1 = adc::Channel::new_pin(p.PIN_28, gpio::Pull::None);
    let mut mux_io_2 = adc::Channel::new_pin(p.PIN_29, gpio::Pull::None);

    spawner
        .spawn(audio_loop(
            p.PWM_SLICE5,
            p.PIN_10,
            p.PIN_11,
            p.SPI0,
            p.PIN_18,
            p.PIN_19,
            p.DMA_CH0,
            p.PIN_21,
        ))
        .unwrap();
    spawner
        .spawn(cv_loop(
            p.PWM_SLICE6,
            p.PIN_12,
            p.PIN_13,
            p.PWM_SLICE3,
            p.PIN_23,
            p.PIN_22,
        ))
        .unwrap();

    let mut mux_state = MuxState::default();
    let snd = WATCH_INPUT.sender();
    let mux_settle_micros = 1;

    // read from physical knobs and switch, write to `mux_state`
    loop {
        // read Main knob & cv1
        muxlogic_a.set_low();
        muxlogic_b.set_low();
        // this seems to need a delay for pins to settle before reading.
        Timer::after_micros(mux_settle_micros).await;

        match mux_adc.read(&mut mux_io_1).await {
            Ok(level) => {
                // info!("M knob: MUX_IO_1 ADC: {}", level);
                mux_state.main_knob = level;
            }
            Err(e) => error!("ADC read failed, while reading Main: {}", e),
        };

        match mux_adc.read(&mut mux_io_2).await {
            Ok(level) => {
                // info!("CV1: MUX_IO_2 ADC: {}", level);
                mux_state.cv1 = level;
            }
            Err(e) => error!("ADC read failed, while reading CV1: {}", e),
        };

        // read X knob & cv2
        // NOTE: X and Y appear to be swapped compared to how I read the logic table
        // not sure why.... :/
        muxlogic_a.set_high();
        muxlogic_b.set_low();
        // this seems to need a delay for pins to settle before reading.
        Timer::after_micros(mux_settle_micros).await;

        match mux_adc.read(&mut mux_io_1).await {
            Ok(level) => {
                // info!("X knob: MUX_IO_1 ADC: {}", level);
                mux_state.x_knob = level;
            }
            Err(e) => error!("ADC read failed, while reading X: {}", e),
        };

        match mux_adc.read(&mut mux_io_2).await {
            Ok(level) => {
                // info!("CV2: MUX_IO_2 ADC: {}", level);
                mux_state.cv2 = level;
            }
            Err(e) => error!("ADC read failed, while reading CV2: {}", e),
        };

        // read Y knob
        muxlogic_a.set_low();
        muxlogic_b.set_high();
        // this seems to need 1us delay for pins to 'settle' before reading.
        Timer::after_micros(mux_settle_micros).await;

        match mux_adc.read(&mut mux_io_1).await {
            Ok(level) => {
                // info!("Y knob: MUX_IO_1 ADC: {}", level);
                mux_state.y_knob = level;
            }
            Err(e) => error!("ADC read failed, while reading Y: {}", e),
        };

        // read Z switch
        muxlogic_a.set_high();
        muxlogic_b.set_high();
        // this seems to need 1us delay for pins to 'settle' before reading.
        Timer::after_micros(mux_settle_micros).await;

        match mux_adc.read(&mut mux_io_1).await {
            Ok(level) => {
                // info!("MUX_IO_1 ADC: {}", level);
                mux_state.zswitch = match level {
                    level if level < 1000 => ZSwitch::Momentary,
                    level if level > 3000 => ZSwitch::On,
                    _ => ZSwitch::Off,
                };
            }
            Err(e) => error!("ADC read failed, while reading Z: {}", e),
        };
        snd.send(mux_state.clone());

        // TODO: extract into task dedicated to pulses
        // update pulses
        match mux_state.zswitch {
            ZSwitch::On | ZSwitch::Momentary => {
                led5.set_high();
                pulse_1_raw_out.set_low();
                led6.set_low();
                pulse_2_raw_out.set_high();
            }
            ZSwitch::Off => {
                led5.set_low();
                pulse_1_raw_out.set_high();
                led6.set_high();
                pulse_2_raw_out.set_low();
            }
        }
        Timer::after_millis(20).await;
    }
}

// TODO: improve LED scaling.
// TODO: probably need to make it exponential?
// Also seems like LEDs 1 & 2 might need different brightness curve than 3 & 4?
fn scale_led_brightness(mut value: u16) -> u16 {
    // can't see the difference between the top half of the scale
    value = value.saturating_div(2);
    // reduce brightness
    value / 5
}

fn clamp_output(mut value: u16) -> u16 {
    if value >= 2048 {
        warn!("clamp_output(): value above limit: {}", value);
        value = 2047;
    }
    value
}

// TODO: read about embassy tasks and peripheral ownership...
// do I need to pass them this way?
#[allow(clippy::too_many_arguments)]
#[embassy_executor::task]
async fn audio_loop(
    led_pwm_slice: peripherals::PWM_SLICE5,
    led1_pin: peripherals::PIN_10,
    led2_pin: peripherals::PIN_11,
    spi0: peripherals::SPI0,
    clk: peripherals::PIN_18,
    mosi: peripherals::PIN_19,
    dma0: peripherals::DMA_CH0,
    cs_pin: peripherals::PIN_21,
) {
    let mut mux_rcv = WATCH_INPUT.anon_receiver();

    // LED setup
    let mut c = pwm::Config::default();
    c.top = 20470; // 11 bit PWM * 10

    let pwm5 = pwm::Pwm::new_output_ab(led_pwm_slice, led1_pin, led2_pin, c.clone());
    let (Some(mut led1), Some(mut led2)) = pwm5.split() else {
        error!("Error setting up LED PWM channels for audio_loop");
        return;
    };

    // DAC setup
    let mut spi = spi::Spi::new_txonly(spi0, clk, mosi, dma0, spi::Config::default());
    let mut cs = Output::new(cs_pin, Level::High);

    // DAC config bits
    // 0: channel select 0 = A, 1 = B
    // 1: unused
    // 2: 0 = 2x gain, 1 = 1x
    // 3: 0 = shutdown channel
    let dac_config_a = 0b0001000000000000u16;
    let dac_config_b = 0b1001000000000000u16;
    let mut dac_buffer: [u8; 2];

    loop {
        if let Some(mux_state) = mux_rcv.try_get() {
            // output 1
            let output_value = clamp_output(2048_u16.saturating_sub(mux_state.main_knob / 2));
            led1.set_duty_cycle_fraction(
                scale_led_brightness(2048_u16.saturating_sub(output_value)),
                2048,
            )
            .unwrap_or_else(|_| {
                error!(
                    "error setting LED 1 PWM to : {}",
                    scale_led_brightness(output_value)
                )
            });
            // write to audio output 1
            // the << 4 >> 4 dance clears out the top four bits,
            // to prepare for setting the config bits
            dac_buffer = ((output_value << 4 >> 4) | dac_config_a).to_be_bytes();
            // debug!(
            //     "audio channel 1: {}, {}: buff: 0x{:08b}{:08b}",
            //     mux_state.main_knob, output_value, dac_buffer[0], dac_buffer[1]
            // );
            cs.set_low();
            spi.blocking_write(&dac_buffer).unwrap();
            cs.set_high();

            // output 2
            // write to audio output 2
            let output_value = clamp_output(mux_state.main_knob / 2);
            led2.set_duty_cycle_fraction(
                scale_led_brightness(2048_u16.saturating_sub(output_value)),
                2047,
            )
            .unwrap_or_else(|_| {
                error!(
                    "error setting LED 2 PWM to : {}",
                    scale_led_brightness(output_value)
                )
            });
            dac_buffer = ((output_value << 4 >> 4) | dac_config_b).to_be_bytes();
            // debug!(
            //     "audio channel 2: {}, {}: buff: 0x{:08b}{:08b}",
            //     mux_state.main_knob, output_value, dac_buffer[0], dac_buffer[1]
            // );
            cs.set_low();
            spi.blocking_write(&dac_buffer).unwrap();
            cs.set_high();
        }
        Timer::after_millis(20).await;
    }
}

#[embassy_executor::task]
async fn cv_loop(
    led_pwm_slice: peripherals::PWM_SLICE6,
    led3_pin: peripherals::PIN_12,
    led4_pin: peripherals::PIN_13,
    cv_pwm_slice: peripherals::PWM_SLICE3,
    cv1_pin: peripherals::PIN_23,
    cv2_pin: peripherals::PIN_22,
) {
    // If we aim for a specific frequency, here is how we can calculate the top value.
    // The top value sets the period of the PWM cycle, so a counter goes from 0 to top and then wraps around to 0.
    // Every such wraparound is one PWM cycle. So here is how we get 60KHz:
    let desired_freq_hz = 60_000;
    let clock_freq_hz = embassy_rp::clocks::clk_sys_freq();
    let divider = 16u8;
    let period = (clock_freq_hz / (desired_freq_hz * divider as u32)) as u16 - 1;

    // CV PWM setup
    // Inverted PWM output. Two pole active filtered. Use 11 bit PWM at 60khz.
    // 2047 = -6v
    // 1024 =  0v
    // 0    = +6v
    let mut cv_pwm_config = pwm::Config::default();
    cv_pwm_config.top = period;
    cv_pwm_config.divider = divider.into();

    let pwm3 = pwm::Pwm::new_output_ab(cv_pwm_slice, cv2_pin, cv1_pin, cv_pwm_config.clone());
    // Yes, cv_2_pwm has the lower GPIO pin.
    let (Some(mut cv2_pwm), Some(mut cv1_pwm)) = pwm3.split() else {
        error!("Error setting up CV PWM channels for cv_loop");
        return;
    };

    // LED PWM setup
    let mut led_pwm_config = pwm::Config::default();
    led_pwm_config.top = 20470; // 11 bit PWM * 10

    let pwm6 = pwm::Pwm::new_output_ab(led_pwm_slice, led3_pin, led4_pin, led_pwm_config.clone());
    let (Some(mut led3), Some(mut led4)) = pwm6.split() else {
        error!("Error setting up LED PWM channels for cv_loop");
        return;
    };
    let mut mux_rcv = WATCH_INPUT.anon_receiver();

    let mut x_output: u16;
    let mut y_output: u16;
    // TODO: decide how to handle these errors when setting PWM.
    loop {
        if let Some(mux_state) = mux_rcv.try_get() {
            // info!("X value: {:?}", mux_state.x_knob);
            x_output = clamp_output(mux_state.x_knob / 2);
            y_output = clamp_output(mux_state.y_knob / 2);

            led3.set_duty_cycle_fraction(scale_led_brightness(x_output), 2047)
                .unwrap_or_else(|_| {
                    error!(
                        "error setting LED 3 PWM to : {}",
                        scale_led_brightness(x_output)
                    )
                });
            led4.set_duty_cycle_fraction(scale_led_brightness(y_output), 2047)
                .unwrap_or_else(|_| {
                    error!(
                        "error setting LED 4 PWM to : {}",
                        scale_led_brightness(y_output)
                    )
                });

            // set CV PWM
            // info!(
            //     "{}, {}, {}",
            //     mux_state.x_knob,
            //     x_output,
            //     2047_u16.saturating_sub(x_output)
            // );
            cv1_pwm
                .set_duty_cycle_fraction(2047_u16.saturating_sub(x_output), 2047)
                .unwrap_or_else(|_| {
                    error!(
                        "error setting CV1 PWM to : {}",
                        2047_u16.saturating_sub(x_output)
                    )
                });
            cv2_pwm
                .set_duty_cycle_fraction(2047_u16.saturating_sub(y_output), 2047)
                .unwrap_or_else(|_| {
                    error!(
                        "error setting CV2 PWM to : {}",
                        2047_u16.saturating_sub(y_output)
                    )
                });
        }
        Timer::after_millis(20).await;
    }
}
