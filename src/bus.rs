//use core::cell::{Cell, RefCell};
use usb_device::{Result, UsbError};
use usb_device::bus::{UsbBusWrapper, PollResult};
use usb_device::endpoint::{EndpointDirection, EndpointType};
use cortex_m::asm::delay;
use cortex_m::interrupt;
use stm32f103xx::{USB, usb};
use stm32f103xx_hal::prelude::*;
use stm32f103xx_hal::rcc;
use stm32f103xx_hal::gpio::{self, gpioa};
use regs::{NUM_ENDPOINTS, PacketMemory, EpReg, EndpointStatus, calculate_count_rx};
use usb_device::utils::FreezableRefCell;

#[derive(Default)]
struct EndpointRecord {
    ep_type: Option<EndpointType>,
    out_valid: bool,
    in_valid: bool,
}

/// USB peripheral driver for STM32F103 microcontrollers.
pub struct UsbBus {
    regs: USB,
    packet_mem: FreezableRefCell<PacketMemory>,
    max_endpoint: FreezableRefCell<usize>,
    endpoints: FreezableRefCell<[EndpointRecord; NUM_ENDPOINTS]>,
}

impl UsbBus {
    /// Constructs a new USB peripheral driver.
    pub fn usb(regs: USB, apb1: &mut rcc::APB1) -> UsbBusWrapper<Self> {
        // TODO: apb1.enr is not public, figure out how this should really interact with the HAL
        // crate

        let _ = apb1;
        interrupt::free(|_| {
            let dp = unsafe { ::stm32f103xx::Peripherals::steal() };
            dp.RCC.apb1enr.modify(|_, w| w.usben().enabled());
        });

        UsbBusWrapper::new(UsbBus {
            regs,
            packet_mem: FreezableRefCell::new(PacketMemory::new()),
            max_endpoint: FreezableRefCell::default(),
            endpoints: FreezableRefCell::default(),
        })
    }

    /// Gets an `UsbBusResetter` which can be used to force a USB reset and re-enumeration from the
    /// device side.
    ///
    /// This is a potentially out-of-spec hack useful mainly for development. Force a reset at the
    /// start of your program to get the host to re-enumerate your device after flashing new
    /// changes.
    pub fn resetter<'a, M>(&'a self,
        clocks: &rcc::Clocks, crh: &mut gpioa::CRH, pa12: gpioa::PA12<M>) -> UsbBusResetter<'a>
    {
        UsbBusResetter {
            bus: self,
            delay: clocks.sysclk().0,
            pa12: pa12.into_push_pull_output(crh),
        }
    }

    fn ep_regs(&self) -> &'static [EpReg; NUM_ENDPOINTS] {
        return unsafe { &*(&self.regs.ep0r as *const usb::EP0R as *const EpReg as *const [EpReg; NUM_ENDPOINTS]) };
    }
}

impl ::usb_device::bus::UsbBus for UsbBus {
    fn alloc_ep(&self, ep_dir: EndpointDirection, ep_addr: Option<u8>, ep_type: EndpointType,
        max_packet_size: u16, _interval: u8) -> Result<u8>
    {
        let mut pmem = self.packet_mem.borrow_mut();
        let mut endpoints = self.endpoints.borrow_mut();

        let ep_addr = ep_addr.map(|a| (a & !0x80) as usize);
        let mut index = ep_addr.unwrap_or(1);

        loop {
            match ep_addr {
                Some(ep_addr) if index != ep_addr => { return Err(UsbError::EndpointTaken); },
                _ => { },
            }

            if index >= NUM_ENDPOINTS {
                return Err(UsbError::EndpointOverflow);
            }

            let ep = &mut endpoints[index];

            match ep.ep_type {
                None => { ep.ep_type = Some(ep_type); },
                Some(t) if t != ep_type => { index += 1; continue; },
                Some(_) => { },
            };

            match ep_dir {
                EndpointDirection::Out if !ep.out_valid => {
                    let (out_size, bits) = calculate_count_rx(max_packet_size as usize)?;

                    let addr_rx = pmem.alloc(out_size)?;
                    let bd = &pmem.descrs()[index];

                    bd.addr_rx.set(addr_rx);
                    bd.count_rx.set(bits as usize);

                    ep.out_valid = true;

                    break;
                },
                EndpointDirection::In if !ep.in_valid => {
                    let addr_tx = pmem.alloc(max_packet_size as usize)?;
                    let bd = &pmem.descrs()[index];

                    bd.addr_tx.set(addr_tx);
                    bd.count_tx.set(0);

                    ep.in_valid = true;

                    break;
                }
                _ => { index += 1; }
            }
        }

        //Ok(Endpoint::new(self, (index as u8) | D::ADDR_BIT, ep_type, max_packet_size, interval))
        Ok((index as u8) | (ep_dir as u8))
    }

    fn enable(&self) {
        let mut max = 0;
        for (index, ep) in self.endpoints.borrow().iter().enumerate() {
            if ep.out_valid || ep.in_valid {
                max = index;
            }
        }

        *self.max_endpoint.borrow_mut() = max;

        self.max_endpoint.freeze();
        self.endpoints.freeze();

        self.regs.cntr.modify(|_, w| w.pdwn().clear_bit());

        // There is a chip specific startup delay. For STM32F103xx it's 1µs and this should wait for
        // at least that long.
        delay(72);

        self.regs.btable.modify(|_, w| unsafe { w.btable().bits(0) });
        self.regs.cntr.modify(|_, w| w.fres().clear_bit());
        self.regs.istr.modify(|_, w| unsafe { w.bits(0) });
    }

    fn reset(&self) {
        self.regs.istr.modify(|_, w| unsafe { w.bits(0) });

        for (index, ep) in self.endpoints.borrow().iter().enumerate() {
            let reg = &self.ep_regs()[index];

            if let Some(ep_type) = ep.ep_type {
                reg.configure(ep_type, index as u8);

                if ep.out_valid {
                    reg.set_stat_rx(EndpointStatus::Valid);
                }

                if ep.in_valid {
                    reg.set_stat_tx(EndpointStatus::Nak);
                }
            }
        }

        self.regs.daddr.modify(|_, w| unsafe { w.ef().set_bit().add().bits(0) });
    }

    fn set_device_address(&self, addr: u8) {
        self.regs.daddr.modify(|_, w| unsafe { w.add().bits(addr as u8) });
    }

    fn poll(&self) -> PollResult {
        let istr = self.regs.istr.read();

        if istr.wkup().bit_is_set() {
            self.regs.istr.modify(|_, w| w.wkup().clear_bit());

            let fnr = self.regs.fnr.read();
            let bits = (fnr.rxdp().bit_is_set() as u8) << 1 | (fnr.rxdm().bit_is_set() as u8);

            match (fnr.rxdp().bit_is_set(), fnr.rxdm().bit_is_set()) {
                (false, false) | (false, true) => {
                    PollResult::Resume
                },
                _ => {
                    // Spurious wakeup event caused by noise
                    PollResult::Suspend
                }
            }
        } else if istr.reset().bit_is_set() {
            self.regs.istr.modify(|_, w| w.reset().clear_bit());

            PollResult::Reset
        } else if istr.susp().bit_is_set() {
            self.regs.istr.modify(|_, w| w.susp().clear_bit());

            PollResult::Suspend
        } else if istr.ctr().bit_is_set() {
            let mut ep_out = 0;
            let mut ep_in_complete = 0;
            let mut ep_setup = 0;
            let mut bit = 1;

            for reg in &self.ep_regs()[0..=*self.max_endpoint.borrow()] {
                let v = reg.read();

                if v.ctr_rx().bit_is_set() {
                    ep_out |= bit;

                    if v.setup().bit_is_set() {
                        ep_setup |= bit;
                    }
                }

                if v.ctr_tx().bit_is_set() {
                    ep_in_complete |= bit;

                    reg.clear_ctr_tx();
                }

                bit <<= 1;
            }

            PollResult::Data { ep_out, ep_in_complete, ep_setup }
        } else {
            PollResult::None
        }
    }

    fn write(&self, ep_addr: u8, buf: &[u8]) -> Result<usize> {
        if ep_addr & 0x80 == 0 {
            return Err(UsbError::InvalidEndpoint);
        }

        let ep = ep_addr & !0x80;

        if ep as usize >= NUM_ENDPOINTS {
            return Err(UsbError::InvalidEndpoint);
        }

        let reg = &self.ep_regs()[ep as usize];

        match reg.read().stat_tx().bits().into() {
            EndpointStatus::Valid => return Err(UsbError::Busy),
            EndpointStatus::Disabled => return Err(UsbError::InvalidEndpoint),
            _ => {},
        };

        let pmem = self.packet_mem.borrow();
        let bd = &pmem.descrs()[ep as usize];

        // TODO: validate len

        pmem.write(bd.addr_tx.get(), buf);
        bd.count_tx.set(buf.len());

        reg.set_stat_tx(EndpointStatus::Valid);

        Ok(buf.len())
    }

    fn read(&self, ep: u8, buf: &mut [u8]) -> Result<usize> {
        if ep & 0x80 != 0 || ep as usize >= NUM_ENDPOINTS {
            return Err(UsbError::InvalidEndpoint);
        }

        let reg = &self.ep_regs()[ep as usize];

        let reg_v = reg.read();

        let status: EndpointStatus = reg_v.stat_rx().bits().into();

        if status == EndpointStatus::Disabled {
            return Err(UsbError::InvalidEndpoint);
        }

        if !reg_v.ctr_rx().bit_is_set() {
            return Err(UsbError::NoData);
        }

        let pmem = self.packet_mem.borrow();
        let bd = &pmem.descrs()[ep as usize];

        let count = bd.count_rx.get() & 0x3f;
        if count > buf.len() {
            return Err(UsbError::BufferOverflow);
        }

        pmem.read(bd.addr_rx.get(), buf);

        reg.clear_ctr_rx();
        reg.set_stat_rx(EndpointStatus::Valid);

        return Ok(count)
    }

    fn stall(&self, ep: u8) {
        interrupt::free(|_| {
            if ep & 0x80 != 0 {
                self.ep_regs()[(ep & !0x80) as usize].set_stat_tx(EndpointStatus::Stall);
            } else {
                self.ep_regs()[ep as usize].set_stat_rx(EndpointStatus::Stall);
            }
        }
    }

    fn unstall(&self, ep: u8) {
        interrupt::free(|_| {
            let reg = &self.ep_regs()[(ep & !0x80) as usize];

            if ep & 0x80 != 0 {
                if reg.read().stat_tx().bits() == EndpointStatus::Stall as u8 {
                    reg.set_stat_tx(EndpointStatus::Nak);
                }
            } else {
                if reg.read().stat_rx().bits() == EndpointStatus::Stall as u8 {
                    reg.set_stat_rx(EndpointStatus::Valid);
                }
            }
        }
    }

    fn suspend(&self) {
        interrupt::free(|_| {
            self.regs.cntr.modify(|_, w| w
                .fsusp().set_bit()
                .lpmode().set_bit());
        }
    }

    fn resume(&self) {
        interrupt::free(|_| {
            self.regs.cntr.modify(|_, w| w
                .fsusp().clear_bit()
                .lpmode().clear_bit());
        });
    }
}

/// Used for forcing a USB device reset and re-enumeration.
pub struct UsbBusResetter<'a> {
    bus: &'a UsbBus,
    delay: u32,
    pa12: gpioa::PA12<gpio::Output<gpio::PushPull>>,
}

impl<'a> UsbBusResetter<'a> {
    /// Force a USB device reset and re-enumeration.
    pub fn reset(&mut self) {
        let pdwn = self.bus.regs.cntr.read().pdwn().bit_is_set();
        self.bus.regs.cntr.modify(|_, w| w.pdwn().set_bit());

        self.pa12.set_low();
        delay(self.delay);

        self.bus.regs.cntr.modify(|_, w| w.pdwn().bit(pdwn));
    }
}