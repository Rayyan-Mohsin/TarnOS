//! 16550 UART driver (COM1, port 0x3F8).
//!
//! Unlike the bare port poke in `earlycon` (kept as the fallback used by
//! panics and boot messages, which must work even before this driver is
//! initialized), this does the real 16550 initialization sequence and
//! checks LSR before every transmit — required on real hardware even
//! though QEMU's emulation tolerates skipping it.
use spin::Mutex;
use x86_64::instructions::port::Port;

use super::{CharDevice, Driver, InterruptHandler};
use crate::arch::x86_64::interrupts;

const COM1_BASE: u16 = 0x3F8;

const IER_RX_AVAILABLE: u8 = 0x01;
const FCR_ENABLE_FIFO_CLEAR: u8 = 0xC7;
const LCR_8N1: u8 = 0x03;
const LCR_DLAB: u8 = 0x80;
const MCR_INIT: u8 = 0x0B; // DTR | RTS | OUT2 (OUT2 gates the IRQ line to the PIC)
const LSR_DATA_READY: u8 = 0x01;
const LSR_THR_EMPTY: u8 = 0x20;

/// Divisor for 38400 baud from the UART's 115200 Hz base clock. Plenty
/// for a debug console; QEMU ignores it entirely but real hardware and
/// serial-over-USB adapters do not.
const BAUD_DIVISOR: u16 = 3;

pub struct Uart16550 {
    data: Port<u8>,
    interrupt_enable: Port<u8>,
    fifo_control: Port<u8>,
    line_control: Port<u8>,
    modem_control: Port<u8>,
    line_status: Port<u8>,
}

impl Uart16550 {
    const fn new(base: u16) -> Self {
        Self {
            data: Port::new(base),
            interrupt_enable: Port::new(base + 1),
            fifo_control: Port::new(base + 2),
            line_control: Port::new(base + 3),
            modem_control: Port::new(base + 4),
            line_status: Port::new(base + 5),
        }
    }

    fn init(&mut self) {
        unsafe {
            self.interrupt_enable.write(0x00u8);

            self.line_control.write(LCR_DLAB);
            let mut divisor_lo = Port::<u8>::new(COM1_BASE);
            let mut divisor_hi = Port::<u8>::new(COM1_BASE + 1);
            divisor_lo.write((BAUD_DIVISOR & 0xFF) as u8);
            divisor_hi.write((BAUD_DIVISOR >> 8) as u8);

            self.line_control.write(LCR_8N1);
            self.fifo_control.write(FCR_ENABLE_FIFO_CLEAR);
            self.modem_control.write(MCR_INIT);
            self.interrupt_enable.write(IER_RX_AVAILABLE);
        }
    }

    fn line_status(&mut self) -> u8 {
        unsafe { self.line_status.read() }
    }
}

impl Driver for Uart16550 {
    fn name(&self) -> &'static str {
        "uart16550(com1)"
    }
}

impl CharDevice for Uart16550 {
    fn write_byte(&mut self, byte: u8) {
        while self.line_status() & LSR_THR_EMPTY == 0 {
            core::hint::spin_loop();
        }
        unsafe {
            self.data.write(byte);
        }
    }

    fn try_read_byte(&mut self) -> Option<u8> {
        if self.line_status() & LSR_DATA_READY != 0 {
            Some(unsafe { self.data.read() })
        } else {
            None
        }
    }
}

impl InterruptHandler for Uart16550 {
    fn handle_irq(&mut self) {
        // Echo every byte immediately as they arrive. This is a stopgap
        // proving the interrupt -> driver path end to end; a later
        // milestone task replaces it with a real async task woken via a
        // `Waker` instead of doing the echo inline in interrupt context.
        while let Some(byte) = self.try_read_byte() {
            self.write_byte(byte);
        }
    }
}

static COM1: Mutex<Uart16550> = Mutex::new(Uart16550::new(COM1_BASE));

fn handle_irq() {
    COM1.lock().handle_irq();
}

/// Initializes COM1 and unmasks IRQ4 at the PIC. Only once the device is
/// actually configured to raise RX-available interrupts (done inside
/// `Uart16550::init`) is it safe to let that line fire.
pub fn init() {
    COM1.lock().init();
    super::register_irq(interrupts::IRQ4, handle_irq);
    interrupts::unmask_irq4();
}

pub fn write_bytes(bytes: &[u8]) {
    COM1.lock().write_bytes(bytes);
}
