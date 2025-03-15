#![no_std]
#![no_main]

use cortex_m_rt::entry;
use defmt::*;

use embassy_executor::Executor;
use embassy_rp::bind_interrupts;
use embassy_rp::clocks;
use embassy_rp::gpio::{self};
// use embassy_rp::interrupt;
use embassy_rp::multicore::{spawn_core1, Stack};
use embassy_rp::peripherals;
use embassy_rp::pwm;
use embassy_rp::pwm::SetDutyCycle;
use embassy_rp::spi;
use embassy_rp::{adc, Peripheral};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::watch::Watch;
use embassy_time::{Duration, Instant, Ticker, Timer};

use audio_codec_algorithms::decode_adpcm_ima_ms;
use gpio::{Level, Output};
use portable_atomic::{AtomicU32, Ordering};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

use wscomp::{JackSample, Sample, SampleUpdate, U12_MAX};

use mutually_exclusive_features::none_or_one_of;
none_or_one_of!("audio_sine", "audio_micro", "audio_2mb", "audio_16mb");

// This is a port of the Backyard Rain Soundscape app from Playdate to the
// Music Thing Modular Workshop System Computer via Rust & Embassy.

// inputs seem to be numbers from 0..4095 (12 bit), sometimes inverted from the thing they represent.
// outputs seem to be numbers from 0..4095 (12 bit), inverted from the thing they represent.

static AUDIO_FREQ_COUNTER: AtomicU32 = AtomicU32::new(0);
static AUDIO_MAX_TICKS: AtomicU32 = AtomicU32::new(0);

bind_interrupts!(struct Irqs {
    ADC_IRQ_FIFO => adc::InterruptHandler;
});

// TODO: troubleshoot AUDIO_MAX_TICKS, seems to be intermittently lagging.
// TODO: review mutexes... maybe only need CriticalSection for cross-CPU data?
// single writer, multple reader

/// [`MuxState`] with most recent values of inputs behind the mux switcher, wrapped in [`Watch`].
///
/// Updated by input_loop(). All inputs except audio and pulse are behind the
/// mux switcher.
static MUX_INPUT: Watch<CriticalSectionRawMutex, MuxState, 2> = Watch::new();

/// Logical rain intensity stored as a [`Sample`], wrapped in [`Watch`].
///
/// Updated by logic_loop().
///
/// ```text
/// Sample::MAX = 100% heavy rain
/// Sample::ZERO = 100% medium rain
/// Sample::MIN = 100% light rain
/// ```
static INTENSITY: Watch<CriticalSectionRawMutex, Sample, 2> = Watch::new();

/// Slow LFO for modulating intensity
static LFO: Watch<CriticalSectionRawMutex, Sample, 2> = Watch::new();
static AUDIO_INPUT: Watch<CriticalSectionRawMutex, AudioState, 2> = Watch::new();
static AUDIO_OUT_SAMPLES: Channel<CriticalSectionRawMutex, DACSamplePair, 1024> = Channel::new();

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
    main_knob: Sample,
    x_knob: Sample,
    y_knob: Sample,
    zswitch: ZSwitch,
    cv1: JackSample,
    cv2: JackSample,
    sequence_counter: usize,
}

impl MuxState {
    fn default() -> Self {
        MuxState {
            main_knob: Sample::new(Sample::CENTER, false),
            x_knob: Sample::new(Sample::CENTER, false),
            y_knob: Sample::new(Sample::CENTER, false),
            zswitch: ZSwitch::default(),
            // CV inputs are not inverted according to docs.  0V reads ~ 2030
            // NOTE: I get inverted data, and ~2060 as 0v
            cv1: JackSample::new(
                Sample::new(Sample::CENTER, true),
                Sample::new(Sample::CENTER, true),
            ),
            cv2: JackSample::new(
                Sample::new(Sample::CENTER, true),
                Sample::new(Sample::CENTER, true),
            ),
            sequence_counter: 0,
        }
    }
}

/// State of audio inputs collected via direct ADC read.
#[derive(Clone, Format)]
struct AudioState {
    audio1: JackSample,
    audio2: JackSample,
}

impl AudioState {
    fn default() -> Self {
        AudioState {
            audio1: JackSample::new(
                Sample::new(Sample::CENTER, true),
                Sample::new(Sample::CENTER, true),
            ),
            audio2: JackSample::new(
                Sample::new(Sample::CENTER, true),
                Sample::new(Sample::CENTER, true),
            ),
        }
    }
}

static EXECUTOR1: StaticCell<Executor> = StaticCell::new();
static mut CORE1_STACK: Stack<{ 1024 * 16 }> = Stack::new();
// static EXECUTOR_HIGH: InterruptExecutor = InterruptExecutor::new();
static EXECUTOR_DEFAULT: StaticCell<Executor> = StaticCell::new();

// #[interrupt]
// unsafe fn SWI_IRQ_1() {
//     EXECUTOR_HIGH.on_interrupt()
// }

#[entry]
fn main() -> ! {
    info!("Starting main()");

    let p = embassy_rp::init(Default::default());

    // // High-priority executor: SWI_IRQ_1, priority level 2
    // interrupt::SWI_IRQ_1.set_priority(Priority::P2);
    // let spawner = EXECUTOR_HIGH.start(interrupt::SWI_IRQ_1);
    // unwrap!(spawner.spawn(audio_loop()));

    // if we can't spawn tasks, panic is the only option? Thus unwrap() OK?

    spawn_core1(
        // must never use CORE1 outside of this executor
        unsafe { p.CORE1.clone_unchecked() },
        unsafe { &mut *core::ptr::addr_of_mut!(CORE1_STACK) },
        move || {
            let executor1 = EXECUTOR1.init(Executor::new());
            executor1.run(|spawner| {
                unwrap!(spawner.spawn(sample_write_loop(
                    p.SPI0, p.PIN_18, p.PIN_19, p.DMA_CH0, p.PIN_21, p.PIN_8, p.PIN_9,
                )))
            })
        },
    );

    // Low priority executor: runs in thread mode, using WFE/SEV
    let executor = EXECUTOR_DEFAULT.init(Executor::new());
    executor.run(|spawner| {
        unwrap!(spawner.spawn(input_loop(
            p.PIN_4, p.PIN_24, p.PIN_25, p.ADC, p.PIN_28, p.PIN_29, p.PIN_27, p.PIN_26,
        )));
        unwrap!(spawner.spawn(periodic_stats()));
        unwrap!(spawner.spawn(mixer_loop()));
        unwrap!(spawner.spawn(logic_loop()));
        unwrap!(spawner.spawn(update_pwm_loop(
            p.PWM_SLICE5,
            p.PIN_10,
            p.PIN_11,
            p.PWM_SLICE6,
            p.PIN_12,
            p.PIN_13,
            p.PWM_SLICE7,
            p.PIN_14,
            p.PIN_15,
            p.PWM_SLICE3,
            p.PIN_23,
            p.PIN_22,
        )));
    })
}

/// Triangle wave - hardcoded for default intensity modulation
struct TriangleWave11 {
    value: i16,
}

impl TriangleWave11 {
    pub fn new() -> Self {
        TriangleWave11 { value: 0 }
    }

    pub fn tick(&mut self) {
        // wrap at 11bit MIN so abs() is never more than positive 11bit value
        if self.value <= -2_i16.pow(11) {
            self.value = 2_i16.pow(11) - 1
        }
        self.value -= 1;
    }

    pub fn current(&self) -> Sample {
        // TODO: troubleshoot, doesn't seem to be as smooth as it should be when
        // mapped to pitch. Also "flickers" at transition occasionally.
        Sample::from((self.value.abs() - 2_i16.pow(10)) / 2)
    }
}

#[embassy_executor::task]
async fn logic_loop() {
    info!("Starting logic_loop()");

    // local persistent intensity value, smoothed using Sample.update()
    let mut smooth_intensity = Sample::from(0_i32);

    let intensity_snd = INTENSITY.sender();
    intensity_snd.send(Sample::new(0, false));

    let mut lfo = TriangleWave11::new();
    let lfo_snd = LFO.sender();
    lfo_snd.send(lfo.current());

    let mut mux_rcv = MUX_INPUT.anon_receiver();
    let mut audio_rcv = AUDIO_INPUT.anon_receiver();

    let mut counter = 0_usize;
    let mut ticker = Ticker::every(Duration::from_hz(480));
    loop {
        counter = counter.wrapping_add(1);

        // update LFO slowly
        if counter % 2_usize.pow(6) == 0 {
            lfo.tick();
            lfo_snd.send(lfo.current());
        }

        // update intensity
        if let Some(mux_state) = mux_rcv.try_get() {
            // map intensity directly to main knob to start
            let mut intensity = mux_state.main_knob;

            if let Some(audio_state) = audio_rcv.try_get() {
                // If cable plugged into audio1 input, then offset that signal
                if let Some(input) = audio_state.audio1.plugged_value() {
                    intensity = *input + intensity;
                } else {
                    // offset by the internal LFO
                    intensity = lfo.current() + intensity;
                }
            }

            smooth_intensity.update(intensity);
            intensity_snd.send(smooth_intensity);
        }
        ticker.next().await
    }
}

/// Rough LED brightness correction
fn led_gamma(value: u16) -> u16 {
    // based on: https://github.com/TomWhitwell/Workshop_Computer/blob/main/Demonstrations%2BHelloWorlds/CircuitPython/mtm_computer.py
    let temp: u32 = value.into();
    ((temp * temp) / U12_MAX as u32).clamp(0, u16::MAX.into()) as u16
}

fn set_led(led: &mut pwm::PwmOutput, value: u16) {
    // TODO: fix error messge (use actual LED #)
    led.set_duty_cycle_fraction(led_gamma(value), wscomp::U12_MAX)
        .unwrap_or_else(|_| error!("error setting LED 3 PWM to : {}", led_gamma(value)));
}

#[allow(clippy::too_many_arguments)]
#[embassy_executor::task]
async fn update_pwm_loop(
    led12_pwm_slice: peripherals::PWM_SLICE5,
    led1_pin: peripherals::PIN_10,
    led2_pin: peripherals::PIN_11,
    led34_pwm_slice: peripherals::PWM_SLICE6,
    led3_pin: peripherals::PIN_12,
    led4_pin: peripherals::PIN_13,
    led56_pwm_slice: peripherals::PWM_SLICE7,
    led5_pin: peripherals::PIN_14,
    led6_pin: peripherals::PIN_15,
    cv_pwm_slice: peripherals::PWM_SLICE3,
    cv1_pin: peripherals::PIN_23,
    cv2_pin: peripherals::PIN_22,
) {
    info!("Starting update_leds_loop()");

    // LED PWM setup
    let mut led_pwm_config = pwm::Config::default();
    // 12 bit PWM * 10. 10x is to increase PWM rate, reducing visible flicker.
    led_pwm_config.top = 40950;

    let pwm5 = pwm::Pwm::new_output_ab(led12_pwm_slice, led1_pin, led2_pin, led_pwm_config.clone());
    let (Some(mut led1), Some(_led2)) = pwm5.split() else {
        error!("Error setting up LED PWM channels for 1 & 2");
        return;
    };

    let pwm6 = pwm::Pwm::new_output_ab(led34_pwm_slice, led3_pin, led4_pin, led_pwm_config.clone());
    let (Some(mut led3), Some(mut led4)) = pwm6.split() else {
        error!("Error setting up LED PWM channels for 3 & 4");
        return;
    };

    let pwm7 = pwm::Pwm::new_output_ab(led56_pwm_slice, led5_pin, led6_pin, led_pwm_config.clone());
    let (Some(mut led5), Some(_led6)) = pwm7.split() else {
        error!("Error setting up LED PWM channels for 5 & 6");
        return;
    };

    // CV setup
    // If we aim for a specific frequency, here is how we can calculate the top value.
    // The top value sets the period of the PWM cycle, so a counter goes from 0 to top and then wraps around to 0.
    // Every such wraparound is one PWM cycle. So here is how we get 60KHz:
    // 60khz target from Computer docs
    let desired_freq_hz = 60_000;
    let clock_freq_hz = embassy_rp::clocks::clk_sys_freq();
    let divider = 16u8;
    let period = (clock_freq_hz / (desired_freq_hz * divider as u32)) as u16 - 1;

    // CV PWM setup
    // Inverted PWM output. Two pole active filtered. Use 12 bit PWM at 60khz.
    // 4095 = -6v
    // 2048 =  0v
    // 0    = +6v
    let mut cv_pwm_config = pwm::Config::default();
    cv_pwm_config.top = period;
    cv_pwm_config.divider = divider.into();

    let pwm3 = pwm::Pwm::new_output_ab(cv_pwm_slice, cv2_pin, cv1_pin, cv_pwm_config.clone());
    // Yes, cv_2_pwm has the lower GPIO pin.
    let (Some(mut cv2_pwm), Some(mut cv1_pwm)) = pwm3.split() else {
        error!("Error setting up CV PWM channels");
        return;
    };

    let mut intensity_rcv = INTENSITY.anon_receiver();
    let mut lfo_rcv = LFO.anon_receiver();

    let mut ticker = Ticker::every(Duration::from_hz(480));
    loop {
        // LEDs
        // set_led(&mut led1, Sample::from(0_i32).to_output_abs());
        // set_led(&mut led3, Sample::from(0_i32).to_output_abs());
        // set_led(&mut led5, Sample::from(0_i32).to_output_abs());

        // left three leds visualize rain intensity

        if let Some(intensity) = intensity_rcv.try_get() {
            // led2 represents heavy rain
            if intensity > Sample::from(0_i32) {
                set_led(&mut led1, intensity.to_output_abs());
            } else {
                set_led(&mut led1, Sample::from(0_i32).to_output_abs());
            }

            // led4 represents medium rain
            set_led(&mut led3, intensity.to_output_abs_inverted());

            // led 6 represents light rain
            if intensity < Sample::from(0_i32) {
                set_led(&mut led5, intensity.to_output_abs());
            } else {
                set_led(&mut led5, Sample::from(0_i32).to_output_abs());
            }

            // set CV1 to intensity
            cv1_pwm
                .set_duty_cycle_fraction(intensity.to_output_inverted(), U12_MAX)
                .unwrap_or_else(|_| {
                    error!(
                        "error setting CV1 PWM to : {}",
                        intensity.to_output_inverted()
                    )
                });

            // set CV2 and LED4 to LFO value
            if let Some(lfo) = lfo_rcv.try_get() {
                set_led(&mut led4, lfo.to_output());
                cv2_pwm
                    .set_duty_cycle_fraction(lfo.to_output_inverted(), U12_MAX)
                    .unwrap_or_else(|_| {
                        error!("error setting CV2 PWM to : {}", lfo.to_output_inverted())
                    });
            };
        }

        ticker.next().await
    }
}

// this loop should probably be moved into a shared library
#[allow(clippy::too_many_arguments)]
#[embassy_executor::task]
async fn input_loop(
    probe_pin: peripherals::PIN_4,
    muxlogic_a_pin: peripherals::PIN_24,
    muxlogic_b_pin: peripherals::PIN_25,
    p_adc: peripherals::ADC,
    mux_io_1_pin: peripherals::PIN_28,
    mux_io_2_pin: peripherals::PIN_29,
    audio1_pin: peripherals::PIN_27,
    audio2_pin: peripherals::PIN_26,
) {
    info!("Starting input_loop()");

    // Normalization probe
    let mut probe = Output::new(probe_pin, Level::Low);

    // audio input setup (used for CV in this card)
    let mut audio1 = adc::Channel::new_pin(audio1_pin, gpio::Pull::None);
    let mut audio2 = adc::Channel::new_pin(audio2_pin, gpio::Pull::None);
    let mut audio_state = AudioState::default();
    let audio_snd = AUDIO_INPUT.sender();

    // Set mux to read switch Z
    let mut muxlogic_a = Output::new(muxlogic_a_pin, Level::Low);
    let mut muxlogic_b = Output::new(muxlogic_b_pin, Level::Low);

    let mut adc_device = adc::Adc::new(p_adc, Irqs, adc::Config::default());
    let mut mux_io_1 = adc::Channel::new_pin(mux_io_1_pin, gpio::Pull::None);
    let mut mux_io_2 = adc::Channel::new_pin(mux_io_2_pin, gpio::Pull::None);

    let mut mux_state = MuxState::default();
    let mux_snd = MUX_INPUT.sender();
    let mux_settle_micros = 20;
    let probe_settle_micros = 200;

    let mut ticker = Ticker::every(Duration::from_hz(60));
    // read from physical knobs, inputs and switch, write to `mux_state`
    loop {
        mux_state.sequence_counter = mux_state.sequence_counter.wrapping_add(1);

        // read audio inputs and normalization probe input
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

        audio_snd.send(audio_state.clone());
        mux_snd.send(mux_state.clone());

        ticker.next().await;
        // yield_now().await;
    }
}

#[embassy_executor::task]
async fn periodic_stats() {
    info!("Starting periodic_stats()");
    debug!("sys clock: {}", clocks::clk_sys_freq());

    let mut mux_rcv = MUX_INPUT.anon_receiver();
    let mut last_sequence: usize = 0;
    let mut last_audio_counter: u32 = 0;
    let mut current_audio_counter: u32;

    let mut ticker = Ticker::every(Duration::from_millis(1000));
    loop {
        current_audio_counter = AUDIO_FREQ_COUNTER.load(Ordering::Relaxed);
        debug!("current_audio_counter: {}", current_audio_counter);
        if let Some(mux_state) = mux_rcv.try_get() {
            info!(
                "rates: input: {}, audio: {} per sec, max: {}",
                mux_state.sequence_counter - last_sequence,
                current_audio_counter - last_audio_counter,
                AUDIO_MAX_TICKS.load(Ordering::Relaxed),
            );
            last_sequence = mux_state.sequence_counter;
        } else {
            info!(
                "rates: audio: {} per sec, max: {}",
                current_audio_counter - last_audio_counter,
                AUDIO_MAX_TICKS.load(Ordering::Relaxed),
            );
        }
        last_audio_counter = current_audio_counter;

        ticker.next().await
    }
}

/// Raw data ready to send to the DAC
struct DACSamplePair {
    pub audio1: u16,
    pub audio2: u16,
}

impl DACSamplePair {
    // DAC config bits
    // 0: channel select 0 = A, 1 = B
    // 1: unused
    // 2: 0 = 2x gain, 1 = 1x
    // 3: 0 = shutdown channel
    const CONFIG1: u16 = 0b0011000000000000u16;
    const CONFIG2: u16 = 0b1011000000000000u16;

    fn new(sample1: u16, sample2: u16) -> Self {
        Self {
            audio1: sample1 << 4 >> 4 | DACSamplePair::CONFIG1,
            audio2: sample2 << 4 >> 4 | DACSamplePair::CONFIG2,
        }
    }
}

#[cfg(feature = "audio_sine")]
mod audio {
    pub const AUDIO_LIGHT: &[u8; 12432] = include_bytes!("../data/sine_light.wav");
    pub const AUDIO_MEDIUM: &[u8; 12432] = include_bytes!("../data/sine_medium.wav");
    pub const AUDIO_HEAVY: &[u8; 12432] = include_bytes!("../data/sine_heavy.wav");
}

#[cfg(feature = "audio_micro")]
mod audio {
    pub const AUDIO_LIGHT: &[u8; 50320] =
        include_bytes!("../data/backyard_rain_light_loop_micro.wav");
    pub const AUDIO_MEDIUM: &[u8; 50320] =
        include_bytes!("../data/backyard_rain_medium_loop_micro.wav");
    pub const AUDIO_HEAVY: &[u8; 50320] =
        include_bytes!("../data/backyard_rain_heavy_loop_micro.wav");
}

// default to "audio_2mb" if no other audio_* feature is set
#[cfg(not(any(
    feature = "audio_sine",
    feature = "audio_micro",
    feature = "audio_16mb"
)))]
mod audio {
    pub const AUDIO_LIGHT: &[u8; 461844] =
        include_bytes!("../data/backyard_rain_light_loop_short.wav");
    pub const AUDIO_MEDIUM: &[u8; 1067054] =
        include_bytes!("../data/backyard_rain_medium_loop_short.wav");
    pub const AUDIO_HEAVY: &[u8; 482464] =
        include_bytes!("../data/backyard_rain_heavy_loop_short.wav");
}

#[cfg(feature = "audio_16mb")]
mod audio {
    pub const AUDIO_LIGHT: &[u8; 4696052] = include_bytes!("../data/backyard_rain_light_loop.wav");
    pub const AUDIO_MEDIUM: &[u8; 7428102] =
        include_bytes!("../data/backyard_rain_medium_loop.wav");
    pub const AUDIO_HEAVY: &[u8; 4053120] = include_bytes!("../data/backyard_rain_heavy_loop.wav");
}

// alternates for testing
// const AUDIO_MEDIUM: &[u8; 123024] = include_bytes!("../data/sine_long.wav");

/// A very simplistic WAVE parser, returns slice of samples in DATA chunk
///
/// Assumes DATA chunk starts at offset 136, which is true for these specific files.
/// Will panic if DATA not found.
fn data_chunk(wav: &[u8]) -> &[u8] {
    let mut offset = 12;
    loop {
        let chunk = &wav[offset..offset + 4];
        let mut length_bytes = [0_u8; 4];
        length_bytes.clone_from_slice(&wav[offset + 4..offset + 8]);
        let length = u32::from_le_bytes(length_bytes) as usize;
        if b"data" != chunk {
            offset += length + 8;
            continue;
        }
        info!("WAV DATA offset, size: {}, {}", offset, length);
        return &wav[offset + 8..length];
    }
}

fn adpcm_to_stream(data: &[u8], sample_offset: usize) -> impl Iterator<Item = i16> + use<'_> {
    const BLOCK_SIZE: usize = 1024;

    // IMA ADPCM files are 4 bits per sample, these files have a consistent
    // 1024 byte block size and the WAV DATA chunk starts at byte 136.
    // It would probably be better to actually parse the WAV files if they
    // were updatable... but... they aren't and this works for now.
    // This is ignoring any data after the end of the last full BLOCK_SIZE..
    // but in theory, IMA ADPCM DATA chunks should be a multiple of BLOCK_SIZE.
    data_chunk(data)
        .chunks_exact(BLOCK_SIZE)
        .cycle()
        .flat_map(|data| {
            let mut adpcm_output_buffer = [0_i16; 2 * BLOCK_SIZE - 7];
            decode_adpcm_ima_ms(data, false, &mut adpcm_output_buffer).unwrap();
            adpcm_output_buffer
        })
        .skip(sample_offset)
}

#[embassy_executor::task]
async fn mixer_loop() {
    info!("Starting mixer_loop()");

    // Create three iterators which produce full range i16 samples by decoding
    // the ADPCM blocks and repeatedly cylcing through the data. Offset the
    // starting samples with prime numbers, so the three buffers don't run out
    // and process a full block at the same time.
    let mut light_samples = adpcm_to_stream(audio::AUDIO_LIGHT, 0);
    let mut medium_samples = adpcm_to_stream(audio::AUDIO_MEDIUM, 277);
    let mut heavy_samples = adpcm_to_stream(audio::AUDIO_HEAVY, 691);

    let mut intensity_rcv = INTENSITY.anon_receiver();
    let mut saw_value = 0u16;

    // TODO: need to smooth intensity changes over time
    // let mut counter = 0_isize;

    loop {
        let mut light = light_samples
            .next()
            .expect("iterator over cycle() returned None somehow?!?!");
        // down sample from 16 to 12 bit
        light >>= 4;
        let light = Sample::from(light);

        let mut medium = medium_samples
            .next()
            .expect("iterator over cycle() returned None somehow?!?!");
        // down sample from 16 to 12 bit
        medium >>= 4;
        let medium = Sample::from(medium);

        let mut heavy = heavy_samples
            .next()
            .expect("iterator over cycle() returned None somehow?!?!");
        // down sample from 16 to 12 bit
        heavy >>= 4;
        let heavy = Sample::from(heavy);

        let mut mixed = medium;
        if let Some(intensity) = intensity_rcv.try_get() {
            match intensity {
                intensity if intensity >= Sample::from(0_i32) => {
                    mixed = medium.scale_inverted(intensity) + heavy.scale(intensity)
                }
                _ => mixed = medium.scale_inverted(intensity.abs()) + light.scale(intensity.abs()),
            }
        }

        // saw from audio output 2, just because
        saw_value += 16;
        if saw_value > U12_MAX {
            saw_value = 0
        };

        let dac_sample = DACSamplePair::new(mixed.to_output(), saw_value);

        // counter += 1;
        // if counter % 2_isize.pow(15) == 0 {
        //     info!("free_capacity(): {}", AUDIO_OUT_SAMPLES.free_capacity());
        // }

        // push samples until channel full then block the loop
        AUDIO_OUT_SAMPLES.send(dac_sample).await;

        // ticker.next().await
    }
}

// ==== ==== CORE1 data and processing ==== ====

/// Audio sample writing loop
///
/// Runs on the second core (CORE1), all shared data must be safe for concurrency.
#[embassy_executor::task]
async fn sample_write_loop(
    spi0: peripherals::SPI0,
    clk: peripherals::PIN_18,
    mosi: peripherals::PIN_19,
    dma0: peripherals::DMA_CH0,
    cs_pin: peripherals::PIN_21,
    pulse1_pin: peripherals::PIN_8, // maybe temp, for measuring sample rate
    pulse2_pin: peripherals::PIN_9,
) {
    info!("Starting sample_write_loop()");
    let mut local_counter = 0u32;
    let mut local_max_ticks = 0u32;
    let mut previous_loop_end = Instant::now();

    // pulse setup
    let mut pulse1 = Output::new(pulse1_pin, Level::High);
    let mut pulse2 = Output::new(pulse2_pin, Level::High);

    // DAC setup
    let mut config = spi::Config::default();
    config.frequency = 8_000_000;

    let mut spi = spi::Spi::new_txonly(spi0, clk, mosi, dma0, config);
    let mut cs = Output::new(cs_pin, Level::High);

    // Since embassy_rp only supports a fixed 1_000_000 hz tick rate, we can
    // only approximate 48_000 hz. Measured at ~ 47_630, with significant jitter.
    // TODO: look into configuring a custom interrupt and running this task
    // from it. (Or maybe even just outside of embassy?)
    let mut ticker = Ticker::every(Duration::from_hz(48_000));
    loop {
        pulse1.toggle();
        pulse2.set_high();
        local_counter += 1;

        if local_counter % 16 == 0 {
            AUDIO_FREQ_COUNTER.store(local_counter, Ordering::Relaxed);
        }

        let dac_sample_pair = AUDIO_OUT_SAMPLES.receive().await;

        cs.set_low();
        spi.blocking_write(&dac_sample_pair.audio1.to_be_bytes())
            .unwrap_or_else(|e| error!("error writing buff a to DAC: {}", e));
        cs.set_high();
        cs.set_low();
        spi.blocking_write(&dac_sample_pair.audio2.to_be_bytes())
            .unwrap_or_else(|e| error!("error writing buff b to DAC: {}", e));
        cs.set_high();

        // update max ticks this loop has ever taken
        let end = Instant::now();
        let diff = end.saturating_duration_since(previous_loop_end);
        // we're just going to hope a tick never takes more than 71.5 hours,
        // and deal with a rollover if it does
        let diff = diff.as_ticks() as u32;
        previous_loop_end = end;
        // Using this local variable to only mess with locks when the values
        // are actually different. Seems to make a small difference... ~15 ticks
        // added to max if updating atomic each loop
        if diff > local_max_ticks {
            // fetch_max() also updates the atomic value to the max
            AUDIO_MAX_TICKS.fetch_max(diff, Ordering::Relaxed);
            local_max_ticks = diff;
        }
        // reset max every second, for better reporting
        if local_counter % 48000 == 0 {
            local_max_ticks = 0;
            AUDIO_MAX_TICKS.store(0, Ordering::Relaxed);
        }

        pulse2.set_low();
        ticker.next().await
    }
}
