#![no_std]
#![no_main]

use defmt::*;
use embassy_executor::Spawner;
use embassy_futures::yield_now;
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

use wscomp::{InputValue, JackValue};

// This is an attempt to learn how use all inputs & outputs of the Music Thing Modular Workshop System Computer via Rust & Embassy.
// The card maps knobs and the switch to manually set voltages.

// inputs seem to be numbers from 0..4096 (12 bit), sometimes inverted from the thing they represent.
// outputs seem to be numbers from 0..2048 (11 bit), sometimes inverted from the thing they represent.

// future features, maybe
// TODO: implement pulse input mixing/logic?
// TODO: move more data strctures and logic into shared wscomp library
// TODO: experiment with task communication to eliminate clone of MuxState
// TODO: consider event based pulse updates: only change pulse outputs on switch change or pulse input edge detection (rather than on a loop)
// TODO: read and use calibration data from EEPROM
// TODO: read about defmt levels and overhead (can we leave logging statements in a release build? What are the effects?)
// TODO: read about embassy tasks and peripheral ownership...
// do I need to pass them this way?

bind_interrupts!(struct Irqs {
    ADC_IRQ_FIFO => adc::InterruptHandler;
});

// single writer, multple reader
static MUX_INPUT: Watch<CriticalSectionRawMutex, MuxState, 2> = Watch::new();
static AUDIO_INPUT: Watch<CriticalSectionRawMutex, AudioState, 2> = Watch::new();

/// The state of the three position Z switch
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

/// State of inputs collected via the ADC mux device.
#[derive(Clone, Format)]
struct MuxState {
    main_knob: InputValue,
    x_knob: InputValue,
    y_knob: InputValue,
    zswitch: ZSwitch,
    cv1: JackValue,
    cv2: JackValue,
    sequence_counter: usize,
}

impl MuxState {
    fn default() -> Self {
        MuxState {
            main_knob: InputValue::new(InputValue::CENTER, false),
            x_knob: InputValue::new(InputValue::CENTER, false),
            y_knob: InputValue::new(InputValue::CENTER, false),
            zswitch: ZSwitch::default(),
            // CV inputs are not inverted according to docs.  0V reads ~ 2030
            // NOTE: I get inverted data, and ~2060 as 0v
            cv1: JackValue::new(
                InputValue::new(InputValue::CENTER, true),
                InputValue::new(InputValue::CENTER, true),
            ),
            cv2: JackValue::new(
                InputValue::new(InputValue::CENTER, true),
                InputValue::new(InputValue::CENTER, true),
            ),
            sequence_counter: 0,
        }
    }
}

/// State of audio inputs collected via direct ADC.
#[derive(Clone, Format)]
struct AudioState {
    audio1: JackValue,
    audio2: JackValue,
}

impl AudioState {
    fn default() -> Self {
        AudioState {
            audio1: JackValue::new(
                InputValue::new(InputValue::CENTER, true),
                InputValue::new(InputValue::CENTER, true),
            ),
            audio2: JackValue::new(
                InputValue::new(InputValue::CENTER, true),
                InputValue::new(InputValue::CENTER, true),
            ),
        }
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("Starting main()");
    let p = embassy_rp::init(Default::default());

    // Normalization probe
    let mut probe = Output::new(p.PIN_4, Level::Low);

    // Set mux to read switch Z
    let mut muxlogic_a = Output::new(p.PIN_24, Level::Low);
    let mut muxlogic_b = Output::new(p.PIN_25, Level::Low);

    let mut adc_device = adc::Adc::new(p.ADC, Irqs, adc::Config::default());
    let mut mux_io_1 = adc::Channel::new_pin(p.PIN_28, gpio::Pull::None);
    let mut mux_io_2 = adc::Channel::new_pin(p.PIN_29, gpio::Pull::None);

    // audio input setup (used for CV in this card)
    let mut audio1 = adc::Channel::new_pin(p.PIN_27, gpio::Pull::None);
    let mut audio2 = adc::Channel::new_pin(p.PIN_26, gpio::Pull::None);

    // if we can't spawn tasks, panic is the only option? Thus unwrap() OK here.
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
    spawner
        .spawn(pulse_loop(p.PIN_14, p.PIN_15, p.PIN_8, p.PIN_9))
        .unwrap();
    spawner.spawn(periodic_stats()).unwrap();

    let mut mux_state = MuxState::default();
    let mux_snd = MUX_INPUT.sender();
    let mut audio_state = AudioState::default();
    let audio_snd = AUDIO_INPUT.sender();
    let mux_settle_micros = 20;
    let probe_settle_micros = 200;

    // read from physical knobs, inputs and switch, write to `mux_state`
    loop {
        mux_state.sequence_counter = mux_state.sequence_counter.wrapping_add(1);

        // read audio inputs and their normalization probe inputs
        match adc_device.read(&mut audio1).await {
            Ok(level) => {
                audio_state.audio1.raw.update(level);
                // info!("audio1: {}, {}", level, mux_state.audio1.to_output());
            }
            Err(e) => error!("ADC read failed, while reading audio1: {}", e),
        };
        match adc_device.read(&mut audio2).await {
            Ok(level) => {
                audio_state.audio2.raw.update(level);
                // info!("audio2: {}, {}", level, mux_state.audio2.to_output());
            }
            Err(e) => error!("ADC read failed, while reading audio2: {}", e),
        };

        probe.set_high();
        Timer::after_micros(mux_settle_micros).await;
        match adc_device.read(&mut audio1).await {
            Ok(level) => {
                audio_state.audio1.probe.update(level);
                // info!("audio1: {}, {}", level, mux_state.audio1.to_output());
            }
            Err(e) => error!("ADC read failed, while reading audio1: {}", e),
        };
        match adc_device.read(&mut audio2).await {
            Ok(level) => {
                audio_state.audio2.probe.update(level);
                // info!("audio2: {}, {}", level, mux_state.audio2.to_output());
            }
            Err(e) => error!("ADC read failed, while reading audio2: {}", e),
        };
        probe.set_low();

        // read Main knob & cv1
        muxlogic_a.set_low();
        muxlogic_b.set_low();
        // this seems to need a delay for pins to settle before reading.
        Timer::after_micros(mux_settle_micros).await;

        match adc_device.read(&mut mux_io_1).await {
            Ok(level) => {
                mux_state.main_knob.update(level);
                // info!("M knob: {}, {}", level, mux_state.main_knob.to_output());
            }
            Err(e) => error!("ADC read failed, while reading Main: {}", e),
        };

        // read cv1 (inverted data)
        match adc_device.read(&mut mux_io_2).await {
            Ok(level) => {
                mux_state.cv1.raw.update(level);
                // info!("cv1: {}, {}", level, mux_state.cv1.raw.to_output());
            }
            Err(e) => error!("ADC read failed, while reading CV1: {}", e),
        };
        probe.set_high();
        Timer::after_micros(probe_settle_micros).await;
        match adc_device.read(&mut mux_io_2).await {
            Ok(level) => {
                mux_state.cv1.probe.update(level);
                // info!("cv1: {}, {}", level, mux_state.cv1.probe.to_output());
            }
            Err(e) => error!("ADC read failed, while reading CV1: {}", e),
        };
        probe.set_low();
        Timer::after_micros(probe_settle_micros).await;

        // read X knob & cv2
        // NOTE: X and Y appear to be swapped compared to how I read the logic table
        // not sure why.... :/
        muxlogic_a.set_high();
        muxlogic_b.set_low();
        // this seems to need a delay for pins to settle before reading.
        Timer::after_micros(mux_settle_micros).await;

        match adc_device.read(&mut mux_io_1).await {
            Ok(level) => {
                mux_state.x_knob.update(level);
                // info!("x knob: {}, {}", level, mux_state.x_knob.to_output());
            }
            Err(e) => error!("ADC read failed, while reading X: {}", e),
        };

        // read cv2 (inverted data)
        match adc_device.read(&mut mux_io_2).await {
            Ok(level) => {
                mux_state.cv2.raw.update(level);
                // info!("cv2: {}, {}", level, mux_state.cv2.raw.to_output());
            }
            Err(e) => error!("ADC read failed, while reading CV2: {}", e),
        };
        probe.set_high();
        Timer::after_micros(probe_settle_micros).await;
        match adc_device.read(&mut mux_io_2).await {
            Ok(level) => {
                mux_state.cv2.probe.update(level);
                // info!("cv2: {}, {}", level, mux_state.cv2.probe.to_output());
            }
            Err(e) => error!("ADC read failed, while reading CV2: {}", e),
        };
        probe.set_low();
        Timer::after_micros(probe_settle_micros).await;

        // read Y knob
        muxlogic_a.set_low();
        muxlogic_b.set_high();
        // this seems to need 1us delay for pins to 'settle' before reading.
        Timer::after_micros(mux_settle_micros).await;

        match adc_device.read(&mut mux_io_1).await {
            Ok(level) => {
                mux_state.y_knob.update(level);
                // info!("y knob: {}, {}", level, mux_state.y_knob.to_output());
            }
            Err(e) => error!("ADC read failed, while reading Y: {}", e),
        };

        // read Z switch
        muxlogic_a.set_high();
        muxlogic_b.set_high();
        // this seems to need 1us delay for pins to 'settle' before reading.
        Timer::after_micros(mux_settle_micros).await;

        match adc_device.read(&mut mux_io_1).await {
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

        mux_snd.send(mux_state.clone());
        audio_snd.send(audio_state.clone());

        // Timer::after_millis(20).await;
        Timer::after_millis(1).await;
        yield_now().await;
    }
}

/// Rough LED brightness correction
fn led_gamma(value: u16) -> u16 {
    // based on: https://github.com/TomWhitwell/Workshop_Computer/blob/main/Demonstrations%2BHelloWorlds/CircuitPython/mtm_computer.py
    let temp: u32 = value.into();
    ((temp * temp) / 2048).clamp(0, u16::MAX.into()) as u16
}

#[embassy_executor::task]
async fn periodic_stats() {
    let mut mux_rcv = MUX_INPUT.anon_receiver();
    let mut last_sequence: usize = 0;
    loop {
        if let Some(mux_state) = mux_rcv.try_get() {
            info!(
                "main loop rate: {} per sec",
                mux_state.sequence_counter - last_sequence
            );
            last_sequence = mux_state.sequence_counter;
        }
        Timer::after_secs(1).await;
    }
}

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
    let mut mux_rcv = MUX_INPUT.anon_receiver();
    let mut audio_rcv = AUDIO_INPUT.anon_receiver();

    // LED setup
    let mut c = pwm::Config::default();
    // 11 bit PWM * 10. 10x is to increase PWM rate, reducing visible flicker.
    c.top = 20470;

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
        if let (Some(mux_state), Some(audio_state)) = (mux_rcv.try_get(), audio_rcv.try_get()) {
            // write to audio outputs
            let mut output_value = mux_state.main_knob;
            // If cable plugged into audio inputs, mix then attenuvert that signal
            match (
                audio_state.audio1.plugged_value(),
                audio_state.audio2.plugged_value(),
            ) {
                (Some(in1), Some(in2)) => {
                    let mix = (*in1 + *in2) / 2;
                    output_value = (mix * output_value) / InputValue::OFFSET;
                }
                (Some(input), None) | (None, Some(input)) => {
                    output_value = (*input * output_value) / InputValue::OFFSET;
                }
                (None, None) => {}
            }

            // the << 4 >> 4 dance clears out the top four bits,
            // to prepare for setting the config bits
            dac_buffer =
                ((output_value.to_output_inverted() << 4 >> 4) | dac_config_a).to_be_bytes();
            // debug!(
            //     "audio channel 1: {}, {}: buff: 0x{:08b}{:08b}",
            //     mux_state.main_knob, output_value, dac_buffer[0], dac_buffer[1]
            // );
            cs.set_low();
            spi.blocking_write(&dac_buffer)
                .unwrap_or_else(|e| error!("error writing to DAC: {}", e));
            cs.set_high();

            dac_buffer = ((output_value.to_output() << 4 >> 4) | dac_config_b).to_be_bytes();
            // debug!(
            //     "audio channel 2: {}, {}: buff: 0x{:08b}{:08b}",
            //     mux_state.main_knob, output_value, dac_buffer[0], dac_buffer[1]
            // );
            cs.set_low();
            spi.blocking_write(&dac_buffer)
                .unwrap_or_else(|e| error!("error writing to DAC: {}", e));
            cs.set_high();

            // audio LEDs
            led1.set_duty_cycle_fraction(led_gamma(output_value.to_output()), 2047)
                .unwrap_or_else(|_| {
                    error!(
                        "error setting LED 1 PWM to : {}",
                        led_gamma(output_value.to_output())
                    )
                });
            led2.set_duty_cycle_fraction(led_gamma(output_value.to_output_inverted()), 2047)
                .unwrap_or_else(|_| {
                    error!(
                        "error setting LED 2 PWM to : {}",
                        led_gamma(output_value.to_output_inverted())
                    )
                });
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
    // 60khz target from Computer docs
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
    // 11 bit PWM * 10. 10x is to increase PWM rate, reducing visible flicker.
    led_pwm_config.top = 20470;

    let pwm6 = pwm::Pwm::new_output_ab(led_pwm_slice, led3_pin, led4_pin, led_pwm_config.clone());
    let (Some(mut led3), Some(mut led4)) = pwm6.split() else {
        error!("Error setting up LED PWM channels for cv_loop");
        return;
    };
    let mut mux_rcv = MUX_INPUT.anon_receiver();

    loop {
        if let Some(mux_state) = mux_rcv.try_get() {
            // cv1 output
            let mut x_value = mux_state.x_knob;
            // info!("x: {}", x_value);
            // If cable plugged into cv1, attenuvert that signal
            if let Some(input_cv) = mux_state.cv1.plugged_value() {
                // info!("x: {}, cv: {}", x_value, input_cv);
                x_value = (*input_cv * x_value) / InputValue::OFFSET;
            }
            cv1_pwm
                .set_duty_cycle_fraction(x_value.to_output_inverted(), 2047)
                .unwrap_or_else(|_| {
                    error!(
                        "error setting CV1 PWM to : {}",
                        x_value.to_output_inverted()
                    )
                });

            // cv2 output
            let mut y_value = mux_state.y_knob;
            // info!(
            //     "y: {}, cv: {}, probe: {}, diff: {}",
            //     y_value.to_output(),
            //     mux_state.cv2.raw.to_output(),
            //     mux_state.cv2.probe.to_output(),
            //     mux_state.cv2.probe.to_clamped() - mux_state.cv2.raw.to_clamped()
            // );
            // If cable plugged into cv2, attenuvert that signal
            if let Some(input_cv) = mux_state.cv2.plugged_value() {
                // info!("y: {}, cv: {}", y_value, input_cv);
                y_value = (*input_cv * y_value) / InputValue::OFFSET;
            }
            cv2_pwm
                .set_duty_cycle_fraction(y_value.to_output_inverted(), 2047)
                .unwrap_or_else(|_| {
                    error!(
                        "error setting CV2 PWM to : {}",
                        y_value.to_output_inverted()
                    )
                });

            // LEDs
            led3.set_duty_cycle_fraction(led_gamma(x_value.to_output()), 2047)
                .unwrap_or_else(|_| {
                    error!(
                        "error setting LED 3 PWM to : {}",
                        led_gamma(x_value.to_output())
                    )
                });
            led4.set_duty_cycle_fraction(led_gamma(y_value.to_output()), 2047)
                .unwrap_or_else(|_| {
                    error!(
                        "error setting LED 4 PWM to : {}",
                        led_gamma(y_value.to_output())
                    )
                });
        }
        Timer::after_millis(20).await;
    }
}

#[embassy_executor::task]
async fn pulse_loop(
    led5_pin: peripherals::PIN_14,
    led6_pin: peripherals::PIN_15,
    pulse1_pin: peripherals::PIN_8,
    pulse2_pin: peripherals::PIN_9,
) {
    let mut led5 = Output::new(led5_pin, Level::Low);
    let mut led6 = Output::new(led6_pin, Level::Low);

    // pulse outputs are inverted
    let mut pulse_1_raw_out = Output::new(pulse1_pin, Level::High);
    let mut pulse_2_raw_out = Output::new(pulse2_pin, Level::High);

    let mut mux_rcv = MUX_INPUT.anon_receiver();

    loop {
        if let Some(mux_state) = mux_rcv.try_get() {
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
        }
        Timer::after_millis(20).await;
    }
}
