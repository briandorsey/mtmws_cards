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

use wscomp::InputValue;

// high level notes...
// This is an attempt to learn how use all inputs & outputs of the Music Thing Modular Workshop System Computer via Rust
// The card maps knobs and the switch to manually set voltages.

// most inputs seem to be numbers from 0..4096 (12 bit), sometimes inverted from the thing they represent.
// most outputs seem to be numbers from 0..2048 (11 bit), sometimes inverted from the thing they represent.

// TODO: speed up input processing loop
// TODO: use normalization probe to only mix when inputs are plugged (needed for attenuation math)
// TODO: smooth analog knob reads, maybe inside InputValue?
// TODO: decide how to handle all unwraps properly
// TODO: review pwm frequencies
// future features
// TODO: implement audio input mixing / attenuation?
// TODO: implement CV input mixing / attenuation?
// TODO: experiment with task communication to eliminate clone of MuxState
// TODO: implement pulse input mixing / attenuation?
// TODO: consider event based pulse updates: only change pulse outputs on switch change or pulse input edge detection (rather than on a loop)
// TODO: read and use calibration data from EEPROM
// TODO: read about defmt levels and overhead (can we leave logging statements in a release build? What are the effects?)

bind_interrupts!(struct Irqs {
    ADC_IRQ_FIFO => adc::InterruptHandler;
});

// single writer, multple reader
static WATCH_INPUT: Watch<CriticalSectionRawMutex, MuxState, 2> = Watch::new();

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
///
/// Input jacks are represented as an Option, only having a value when a
/// cable is plugged in. (This doesn't allow reading the value of disconnected
/// inputs... is that ever useful?)
#[derive(Clone, Format)]
struct MuxState {
    main_knob: InputValue,
    x_knob: InputValue,
    y_knob: InputValue,
    zswitch: ZSwitch,
    cv1: InputValue,
    cv2: InputValue,
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
            cv1: InputValue::new(InputValue::CENTER, true),
            cv2: InputValue::new(InputValue::CENTER, true),
            sequence_counter: 0,
        }
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("Starting main()");
    let p = embassy_rp::init(Default::default());
    let mut led5 = Output::new(p.PIN_14, Level::Low);
    let mut led6 = Output::new(p.PIN_15, Level::Low);

    // Normalization probe
    let mut _probe = Output::new(p.PIN_4, Level::Low);

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
    spawner.spawn(periodic_stats()).unwrap();

    let mut mux_state = MuxState::default();
    let snd = WATCH_INPUT.sender();
    let mux_settle_micros = 1;

    // read from physical knobs and switch, write to `mux_state`
    loop {
        mux_state.sequence_counter = mux_state.sequence_counter.wrapping_add(1);

        // every X loops, check input jacks for plugged in cables
        // if mux_state.sequence_counter % 8 == 0 {
        //     match check_normalization(
        //         &mut mux_state,
        //         &mut muxlogic_a,
        //         &mut muxlogic_b,
        //         &mut mux_adc,
        //         &mut mux_io_2,
        //         &mut probe,
        //     )
        //     .await
        //     {
        //         Ok(()) => {}
        //         Err(e) => error!(
        //             "ADC read failed, while reading normalization probe values: {}",
        //             e
        //         ),
        //     }
        // }

        // ---- begin default read section
        // read Main knob & cv1
        muxlogic_a.set_low();
        muxlogic_b.set_low();
        // this seems to need a delay for pins to settle before reading.
        Timer::after_micros(mux_settle_micros).await;

        match mux_adc.read(&mut mux_io_1).await {
            Ok(level) => {
                // info!("M knob: MUX_IO_1 ADC: {}", level);
                mux_state.main_knob.update(level);
            }
            Err(e) => error!("ADC read failed, while reading Main: {}", e),
        };

        // read cv1 (inverted data)
        match mux_adc.read(&mut mux_io_2).await {
            Ok(level) => {
                // info!("CV1: MUX_IO_2 ADC: {}", level);
                mux_state.cv1.update(level);
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
                mux_state.x_knob.update(level);
            }
            Err(e) => error!("ADC read failed, while reading X: {}", e),
        };

        // read cv2 (inverted data)
        match mux_adc.read(&mut mux_io_2).await {
            Ok(level) => {
                // info!("CV2: MUX_IO_2 ADC: {}", level);
                mux_state.cv2.update(level);
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
                mux_state.y_knob.update(level);
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

        // Timer::after_nanos(1).await;
        yield_now().await;
    }
}

async fn _check_normalization<'a, M: adc::Mode>(
    _mux_state: &mut MuxState,
    muxlogic_a: &mut Output<'a>,
    muxlogic_b: &mut Output<'a>,
    mux_adc: &mut adc::Adc<'a, M>,
    mux_io_2: &mut adc::Channel<'a>,
    probe: &mut Output<'a>,
) -> Result<(), adc::Error> {
    muxlogic_a.set_low();
    muxlogic_b.set_low();
    let _level_raw = mux_adc.blocking_read(mux_io_2)?;
    // let _level_raw = InputValue::from_u16_inverted(level_raw);
    probe.set_high();
    Timer::after_micros(1).await;
    let _level_norm = mux_adc.blocking_read(mux_io_2)?;
    // let _level_norm = InputValue::from_u16_inverted(level_norm);

    // info!(
    //     "probe before {}, after: {}, diff: {}",
    //     level_raw,
    //     level_norm,
    //     level_raw - level_norm
    // );

    // cleanup
    probe.set_low();
    muxlogic_a.set_low();
    muxlogic_b.set_low();
    Ok(())
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

#[embassy_executor::task]
async fn periodic_stats() {
    let mut mux_rcv = WATCH_INPUT.anon_receiver();
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
            led1.set_duty_cycle_fraction(
                scale_led_brightness(mux_state.main_knob.to_output()),
                2047,
            )
            .unwrap_or_else(|_| {
                error!(
                    "error setting LED 1 PWM to : {}",
                    scale_led_brightness(mux_state.main_knob.to_output())
                )
            });
            // write to audio output 1
            let output_value = mux_state.main_knob.to_output_inverted();
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
            led2.set_duty_cycle_fraction(
                scale_led_brightness(mux_state.main_knob.to_output_inverted()),
                2047,
            )
            .unwrap_or_else(|_| {
                error!(
                    "error setting LED 2 PWM to : {}",
                    scale_led_brightness(mux_state.main_knob.to_output_inverted())
                )
            });
            let output_value = mux_state.main_knob.to_output();
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

    // TODO: decide how to handle these errors when setting PWM.
    loop {
        if let Some(mux_state) = mux_rcv.try_get() {
            // info!("X value: {:?}", mux_state.x_knob);

            led3.set_duty_cycle_fraction(scale_led_brightness(mux_state.x_knob.to_output()), 2047)
                .unwrap_or_else(|_| {
                    error!(
                        "error setting LED 3 PWM to : {}",
                        scale_led_brightness(mux_state.x_knob.to_output())
                    )
                });
            led4.set_duty_cycle_fraction(scale_led_brightness(mux_state.y_knob.to_output()), 2047)
                .unwrap_or_else(|_| {
                    error!(
                        "error setting LED 4 PWM to : {}",
                        scale_led_brightness(mux_state.y_knob.to_output())
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
                .set_duty_cycle_fraction(mux_state.x_knob.to_output_inverted(), 2047)
                .unwrap_or_else(|_| {
                    error!(
                        "error setting CV1 PWM to : {}",
                        mux_state.x_knob.to_output_inverted()
                    )
                });

            // prototype attenuverting logic
            let y_value = mux_state.y_knob;
            // if let Some(input_cv) = mux_state.cv2 {
            //     y_value = y_value * input_cv / InputValue::OFFSET;
            // }
            cv2_pwm
                .set_duty_cycle_fraction(y_value.to_output_inverted(), 2047)
                .unwrap_or_else(|_| {
                    error!(
                        "error setting CV2 PWM to : {}",
                        y_value.to_output_inverted()
                    )
                });
        }
        Timer::after_millis(20).await;
    }
}
