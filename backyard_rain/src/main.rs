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
// use embassy_rp::pwm;
// use embassy_rp::pwm::SetDutyCycle;
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

use wscomp::{InputValue, JackValue};

// This is a port of the Backyard Rain Soundscape app from Playdate to the
// Music Thing Modular Workshop System Computer via Rust & Embassy.

// inputs seem to be numbers from 0..4096 (12 bit), sometimes inverted from the thing they represent.
// outputs seem to be numbers from 0..2048 (11 bit), sometimes inverted from the thing they represent.

static AUDIO_FREQ_COUNTER: AtomicU32 = AtomicU32::new(0);
static AUDIO_MAX_TICKS: AtomicU32 = AtomicU32::new(0);

bind_interrupts!(struct Irqs {
    ADC_IRQ_FIFO => adc::InterruptHandler;
});

// single writer, multple reader
static MUX_INPUT: Watch<CriticalSectionRawMutex, MuxState, 2> = Watch::new();
// static AUDIO_INPUT: Watch<CriticalSectionRawMutex, AudioState, 2> = Watch::new();
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
                unwrap!(spawner.spawn(audio_loop(
                    p.SPI0, p.PIN_18, p.PIN_19, p.DMA_CH0, p.PIN_21, p.PIN_8, p.PIN_9,
                )))
            })
        },
    );

    // Low priority executor: runs in thread mode, using WFE/SEV
    let executor = EXECUTOR_DEFAULT.init(Executor::new());
    executor.run(|spawner| {
        unwrap!(spawner.spawn(input_loop(
            p.PIN_4, p.PIN_24, p.PIN_25, p.ADC, p.PIN_28, p.PIN_29,
        )));
        unwrap!(spawner.spawn(periodic_stats()));
        unwrap!(spawner.spawn(mixer_loop()));
    })
}

// this loop should probably be moved into a shared library
#[embassy_executor::task]
async fn input_loop(
    probe_pin: peripherals::PIN_4,
    muxlogic_a_pin: peripherals::PIN_24,
    muxlogic_b_pin: peripherals::PIN_25,
    p_adc: peripherals::ADC,
    mux_io_1_pin: peripherals::PIN_28,
    mux_io_2_pin: peripherals::PIN_29,
) {
    info!("Starting input_loop()");

    // Normalization probe
    let mut probe = Output::new(probe_pin, Level::Low);

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

        ticker.next().await;
        // yield_now().await;
    }
}

/// Rough LED brightness correction
fn _led_gamma(value: u16) -> u16 {
    // based on: https://github.com/TomWhitwell/Workshop_Computer/blob/main/Demonstrations%2BHelloWorlds/CircuitPython/mtm_computer.py
    let temp: u32 = value.into();
    ((temp * temp) / 2048).clamp(0, u16::MAX.into()) as u16
}

#[embassy_executor::task]
async fn periodic_stats() {
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
                "rates: main: {}, audio: {} per sec, max: {}",
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
    const CONFIG1: u16 = 0b0001000000000000u16;
    const CONFIG2: u16 = 0b1001000000000000u16;

    fn new(sample1: u16, sample2: u16) -> Self {
        Self {
            audio1: sample1 << 4 >> 4 | DACSamplePair::CONFIG1,
            audio2: sample2 << 4 >> 4 | DACSamplePair::CONFIG2,
        }
    }
}

#[embassy_executor::task]
async fn mixer_loop() {
    info!("Starting mixer_loop()");

    const BLOCK_SIZE: usize = 1024;
    // IMA ADPCM files are 4 bits per sample, grab data a byte at a time
    let mut medium_samples = AUDIO_MEDIUM[136 + 8..]
        // TODO: hardcoded for now...
        .chunks_exact(BLOCK_SIZE)
        .cycle()
        .flat_map(|data| {
            let mut adpcm_output_buffer = [0_i16; 2 * BLOCK_SIZE - 7];
            decode_adpcm_ima_ms(data, false, &mut adpcm_output_buffer).unwrap();
            adpcm_output_buffer
        });

    let mut saw_value = 0u16;

    loop {
        let mut sample = medium_samples
            .next()
            .expect("iterator over cycle returned None somehow?!?!");
        // down sample from 16 to 12 bit
        sample >>= 4;
        defmt::assert!((-2048..2048).contains(&sample), "12 bit, was: {}", sample);
        // down sample 1 more bit to 11 bit
        sample >>= 1;
        defmt::assert!((-1024..1024).contains(&sample), "11 bit, was: {}", sample);
        // convert to u16
        let mut sample: u16 = if sample > 0 {
            sample as u16 + 1024u16
        } else {
            (sample + 1024) as u16
        };
        defmt::assert!((0..2048).contains(&sample), "11 bit u16, was: {}", sample);
        // 11 bit invert
        sample = 2047 - sample;
        // clear the left four bits
        sample = (sample << 4) >> 4;
        defmt::assert!(sample <= 2047, "was: {}", sample);

        // saw from audio output 2, just because
        saw_value += 8;
        if saw_value > 2047 {
            saw_value = 0
        };

        let dac_sample = DACSamplePair::new(sample, saw_value);

        // push samples until channel full then block the loop
        AUDIO_OUT_SAMPLES.send(dac_sample).await;

        // ticker.next().await
    }
}

// ==== ==== CORE1 data and processing ==== ====
// const AUDIO_HEAVY: &[u8; 48044] = include_bytes!("../data/sine_48_440.wav");
// const AUDIO_MEDIUM: &[u8; 12432] = include_bytes!("../data/sine_medium.wav");
// const AUDIO_LIGHT: &[u8; 12432] = include_bytes!("../data/sine_light.wav");

// const AUDIO_MEDIUM: &[u8; 123024] = include_bytes!("../data/sine_long.wav");
const AUDIO_MEDIUM: &[u8; 441488] = include_bytes!("../data/backyard_thunder_01.wav");

/// Audio processing loop
///
/// Runs on the second core (CORE1), all shared data must be safe for concurrency.
#[embassy_executor::task]
async fn audio_loop(
    spi0: peripherals::SPI0,
    clk: peripherals::PIN_18,
    mosi: peripherals::PIN_19,
    dma0: peripherals::DMA_CH0,
    cs_pin: peripherals::PIN_21,
    pulse1_pin: peripherals::PIN_8, // maybe temp, for measuring sample rate
    pulse2_pin: peripherals::PIN_9,
) {
    info!("Starting audio_loop()");
    let mut local_counter = 0u32;
    let mut local_max_ticks = 0u32;
    let mut previous_loop_end = Instant::now();

    let mut pulse1 = Output::new(pulse1_pin, Level::High);
    let mut pulse2 = Output::new(pulse2_pin, Level::High);

    let mut config = spi::Config::default();
    config.frequency = 8_000_000;

    // DAC setup
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

        // manually handling samples above... consider using InputValue
        // let sample = InputValue::from_i16(sample, false);
        // dac_buffer = (sample.to_output_inverted() | dac_config_a).to_be_bytes();

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
