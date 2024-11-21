#![no_std]
#![no_main]

use core::cell::RefCell;

use blus_fw::*;
use defmt::{debug, info, unwrap};
use embassy_executor::Spawner;
use embassy_stm32::time::Hertz;
use embassy_stm32::{bind_interrupts, i2c, interrupt, peripherals, timer, usb};
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::zerocopy_channel;
use embassy_usb::class::uac1;
use embassy_usb::class::uac1::speaker::{self, Speaker};
use heapless::Vec;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    OTG_FS => usb::InterruptHandler<peripherals::USB_OTG_FS>;
    I2C1_EV => i2c::EventInterruptHandler<peripherals::I2C1>;
    I2C1_ER => i2c::ErrorInterruptHandler<peripherals::I2C1>;
});

static TIMER: Mutex<CriticalSectionRawMutex, RefCell<Option<timer::low_level::Timer<peripherals::TIM2>>>> =
    Mutex::new(RefCell::new(None));

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("Hi.");

    let mut peripheral_config = embassy_stm32::Config::default();
    {
        // Uses a 24.576 MHz external oscillator.
        use embassy_stm32::rcc::*;
        peripheral_config.rcc.hse = Some(Hse {
            freq: Hertz(25_000_000),
            mode: HseMode::Bypass,
        });
        peripheral_config.rcc.sys = Sysclk::PLL1_P;

        peripheral_config.rcc.ahb_pre = AHBPrescaler::DIV1;
        peripheral_config.rcc.apb1_pre = APBPrescaler::DIV2;
        peripheral_config.rcc.apb2_pre = APBPrescaler::DIV2;

        peripheral_config.rcc.mux.clk48sel = mux::Clk48sel::PLL1_Q;

        peripheral_config.rcc.pll_src = PllSource::HSE;
        peripheral_config.rcc.pll = Some(Pll {
            prediv: PllPreDiv::DIV16,
            mul: PllMul::MUL250,
            divp: Some(PllPDiv::DIV6),
            divq: Some(PllQDiv::DIV8),
            divr: None,
        });

        peripheral_config.rcc.plli2s = Some(Pll {
            prediv: PllPreDiv::DIV16,
            mul: PllMul::MUL192,
            divp: None,
            divq: None,
            divr: Some(PllRDiv::DIV2),
        });
    }
    let p = embassy_stm32::init(peripheral_config);

    let mut core_peri = cortex_m::Peripherals::take().unwrap();

    // Enable instruction cache.
    core_peri.SCB.enable_icache();

    debug!("USB packet size is {} byte", USB_MAX_PACKET_SIZE);
    static CONFIG_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
    let config_descriptor = CONFIG_DESCRIPTOR.init([0; 256]);

    static BOS_DESCRIPTOR: StaticCell<[u8; 32]> = StaticCell::new();
    let bos_descriptor = BOS_DESCRIPTOR.init([0; 32]);

    const CONTROL_BUF_SIZE: usize = 64;
    static CONTROL_BUF: StaticCell<[u8; CONTROL_BUF_SIZE]> = StaticCell::new();
    let control_buf = CONTROL_BUF.init([0; CONTROL_BUF_SIZE]);

    const FEEDBACK_BUF_SIZE: usize = 4;
    static EP_OUT_BUFFER: StaticCell<[u8; FEEDBACK_BUF_SIZE + CONTROL_BUF_SIZE + USB_MAX_PACKET_SIZE]> =
        StaticCell::new();
    let ep_out_buffer = EP_OUT_BUFFER.init([0u8; FEEDBACK_BUF_SIZE + CONTROL_BUF_SIZE + USB_MAX_PACKET_SIZE]);

    static STATE: StaticCell<speaker::State> = StaticCell::new();
    let state = STATE.init(speaker::State::new());

    // Create the driver, from the HAL.
    let mut usb_config = usb::Config::default();

    usb_config.vbus_detection = true;

    // Initialize driver for high-speed external PHY.
    let usb_driver = usb::Driver::new_fs(p.USB_OTG_FS, Irqs, p.PA12, p.PA11, ep_out_buffer, usb_config);

    // Basic USB device configuration
    let mut config = embassy_usb::Config::new(0x1209, 0xaf02);
    config.manufacturer = Some("elagil");
    config.product = Some("testing");
    config.self_powered = true;
    config.max_power = 0;

    // Required for windows compatibility.
    // https://developer.nordicsemi.com/nRF_Connect_SDK/doc/1.9.1/kconfig/CONFIG_CDC_ACM_IAD.html#help
    config.device_class = 0xEF;
    config.device_sub_class = 0x02;
    config.device_protocol = 0x01;
    config.composite_with_iads = true;

    let mut builder = embassy_usb::Builder::new(
        usb_driver,
        config,
        config_descriptor,
        bos_descriptor,
        &mut [], // no msos descriptors
        control_buf,
    );

    // Create the UAC1 Speaker class components
    let (stream, feedback, control_changed) = Speaker::new(
        &mut builder,
        state,
        USB_MAX_PACKET_SIZE as u16,
        uac1::SampleWidth::Width4Byte,
        &[SAMPLE_RATE_HZ],
        &AUDIO_CHANNELS,
        FEEDBACK_REFRESH_PERIOD,
    );

    // Build and run the USB device
    let usb_device = builder.build();

    // Establish a zero-copy channel for transferring received audio samples from the USB audio task.
    static USB_SAMPLE_BLOCKS: StaticCell<[UsbSampleBlock; 2]> = StaticCell::new();
    let usb_sample_blocks = USB_SAMPLE_BLOCKS.init([Vec::new(), Vec::new()]);

    static USB_CHANNEL: StaticCell<zerocopy_channel::Channel<'_, NoopRawMutex, UsbSampleBlock>> = StaticCell::new();
    let usb_channel = USB_CHANNEL.init(zerocopy_channel::Channel::new(usb_sample_blocks));
    let (usb_sender, _usb_receiver) = usb_channel.split();

    // Trigger on USB SOF (internal signal)
    let mut tim2 = timer::low_level::Timer::new(p.TIM2);
    tim2.set_tick_freq(Hertz(FEEDBACK_COUNTER_TICK_RATE));
    tim2.set_trigger_source(timer::low_level::TriggerSource::ITR1);

    const CHANNEL: timer::Channel = timer::Channel::Ch1;
    tim2.set_input_ti_selection(CHANNEL, timer::low_level::InputTISelection::TRC);
    tim2.set_input_capture_prescaler(CHANNEL, 0);
    tim2.set_input_capture_filter(CHANNEL, timer::low_level::FilterValue::FCK_INT_N2);

    // Reset all interrupt flags.
    tim2.regs_gp32().sr().write(|r| r.0 = 0);

    // Enable routing of SOF to the timer.
    tim2.regs_gp32().or().write(|r| *r = 0b10 << 10);

    tim2.enable_channel(CHANNEL, true);
    tim2.enable_input_interrupt(CHANNEL, true);

    tim2.start();

    TIMER.lock(|p| p.borrow_mut().replace(tim2));

    unsafe {
        cortex_m::peripheral::NVIC::unmask(interrupt::TIM2);
    }

    // Launch USB audio tasks.
    unwrap!(spawner.spawn(usb_audio::control_task(control_changed)));
    unwrap!(spawner.spawn(usb_audio::streaming_task(stream, usb_sender)));
    unwrap!(spawner.spawn(usb_audio::feedback_task(feedback)));
    unwrap!(spawner.spawn(usb_audio::usb_task(usb_device)));
}

#[interrupt]
fn TIM2() {
    static mut LAST_TICKS: u32 = 0;
    static mut FRAME_COUNT: usize = 0;

    critical_section::with(|cs| {
        // Read timer counter.
        let timer = TIMER.borrow(cs).borrow().as_ref().unwrap().regs_gp32();

        let status = timer.sr().read();

        const CHANNEL_INDEX: usize = 0;
        if status.ccif(CHANNEL_INDEX) {
            let ticks = timer.ccr(CHANNEL_INDEX).read();

            *FRAME_COUNT += 1;
            if *FRAME_COUNT >= FEEDBACK_REFRESH_PERIOD.frame_count() {
                *FRAME_COUNT = 0;
                FEEDBACK_SIGNAL.signal(ticks.wrapping_sub(*LAST_TICKS));
                *LAST_TICKS = ticks;
            }
        };

        // Clear trigger interrupt flag.
        timer.sr().modify(|r| r.set_tif(false));
    });
}