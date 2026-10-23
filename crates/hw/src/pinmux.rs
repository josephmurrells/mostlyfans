use crate::HwError;
use crate::port::PortIo;
use crate::superio::ConfigSpace;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wiring {
    tachs: u8,
    pwms: u8,
}

impl Wiring {
    pub const ALL: Self = Self {
        tachs: u8::MAX,
        pwms: u8::MAX,
    };

    pub fn has_tach(self, channel: usize) -> bool {
        has(self.tachs, channel)
    }

    pub fn has_pwm(self, channel: usize) -> bool {
        has(self.pwms, channel)
    }

    pub fn is_wired(self, channel: usize) -> bool {
        self.has_tach(channel) || self.has_pwm(channel)
    }

    pub(crate) fn without_pwm(self, channel: usize) -> Self {
        Self {
            pwms: self.pwms & !(1 << channel),
            ..self
        }
    }

    fn without(self, channel: usize, pin: Pin) -> Self {
        let bit = !(1 << channel);
        match pin {
            Pin::Tach => Self {
                tachs: self.tachs & bit,
                ..self
            },
            Pin::Pwm => self.without_pwm(channel),
            Pin::Both => Self {
                tachs: self.tachs & bit,
                pwms: self.pwms & bit,
            },
        }
    }
}

fn has(mask: u8, channel: usize) -> bool {
    channel < 8 && mask & (1 << channel) != 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pin {
    Tach,
    Pwm,
    Both,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rule {
    channel: usize,
    pin: Pin,
    register: u8,
    bit: u8,
    wired_when_set: bool,
}

const fn set(channel: usize, pin: Pin, register: u8, bit: u8) -> Rule {
    Rule {
        channel,
        pin,
        register,
        bit,
        wired_when_set: true,
    }
}

const fn clear(channel: usize, pin: Pin, register: u8, bit: u8) -> Rule {
    Rule {
        wired_when_set: false,
        ..set(channel, pin, register, bit)
    }
}

const ITE_GPIO_LDN: u8 = 0x07;
const GPIO1: u8 = 0x25;
const GPIO2: u8 = 0x26;
const GPIO3: u8 = 0x27;
const GPIO4: u8 = 0x28;
const GPIO5: u8 = 0x29;
const PINX4: u8 = 0x2D;
const GPIO9: u8 = 0xD3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IteMux {
    It8620,
    It8625,
    It8655,
    It8665,
    Classic,
}

const IT8620_RULES: &[Rule] = &[
    clear(4, Pin::Pwm, GPIO1, 6),
    set(3, Pin::Tach, GPIO2, 5),
    set(4, Pin::Tach, GPIO2, 4),
    clear(2, Pin::Pwm, GPIO3, 6),
    clear(2, Pin::Tach, GPIO3, 7),
    clear(3, Pin::Pwm, GPIO4, 2),
    clear(1, Pin::Pwm, GPIO5, 1),
    clear(1, Pin::Tach, GPIO5, 2),
    set(5, Pin::Both, GPIO5, 7),
];

const IT8625_RULES: &[Rule] = &[
    clear(1, Pin::Pwm, GPIO5, 1),
    clear(1, Pin::Tach, GPIO5, 2),
    clear(2, Pin::Pwm, GPIO3, 6),
    clear(2, Pin::Tach, GPIO3, 7),
    clear(3, Pin::Pwm, GPIO1, 5),
    clear(3, Pin::Tach, GPIO1, 6),
    clear(4, Pin::Pwm, GPIO3, 3),
    set(4, Pin::Tach, GPIO3, 1),
    clear(5, Pin::Pwm, GPIO9, 0),
    clear(5, Pin::Tach, GPIO9, 1),
];

const IT8655_RULES: &[Rule] = &[
    clear(1, Pin::Pwm, GPIO5, 1),
    clear(1, Pin::Tach, PINX4, 4),
    clear(2, Pin::Pwm, GPIO3, 6),
    clear(2, Pin::Tach, GPIO3, 7),
];

const IT8665_RULES: &[Rule] = &[
    clear(1, Pin::Pwm, GPIO5, 1),
    clear(1, Pin::Tach, PINX4, 4),
    clear(2, Pin::Pwm, GPIO3, 6),
    clear(2, Pin::Tach, GPIO3, 7),
    clear(3, Pin::Pwm, GPIO9, 2),
    clear(3, Pin::Tach, GPIO9, 3),
    clear(4, Pin::Pwm, GPIO2, 5),
    set(4, Pin::Tach, GPIO2, 4),
    clear(5, Pin::Pwm, GPIO9, 0),
    clear(5, Pin::Tach, GPIO9, 1),
];

const CLASSIC_RULES: &[Rule] = &[
    clear(1, Pin::Pwm, GPIO5, 1),
    clear(1, Pin::Tach, GPIO5, 2),
    clear(2, Pin::Pwm, GPIO3, 6),
    clear(2, Pin::Tach, GPIO3, 7),
];

impl IteMux {
    fn rules(self) -> &'static [Rule] {
        match self {
            Self::It8620 => IT8620_RULES,
            Self::It8625 => IT8625_RULES,
            Self::It8655 => IT8655_RULES,
            Self::It8665 => IT8665_RULES,
            Self::Classic => CLASSIC_RULES,
        }
    }
}

pub(crate) fn ite_wiring<P: PortIo>(
    space: &mut ConfigSpace<'_, P>,
    mux: IteMux,
    channels: usize,
) -> Result<Wiring, HwError> {
    space.select(ITE_GPIO_LDN)?;
    let mut values: Vec<(u8, u8)> = Vec::new();
    for rule in mux.rules().iter().filter(|rule| rule.channel < channels) {
        if !values
            .iter()
            .any(|(register, _)| *register == rule.register)
        {
            values.push((rule.register, space.read(rule.register)?));
        }
    }
    Ok(apply_rules(mux.rules(), channels, |register| {
        values
            .iter()
            .find(|(read, _)| *read == register)
            .map_or(0, |&(_, value)| value)
    }))
}

fn apply_rules(rules: &[Rule], channels: usize, value: impl Fn(u8) -> u8) -> Wiring {
    rules
        .iter()
        .filter(|rule| rule.channel < channels)
        .filter(|rule| (value(rule.register) & (1 << rule.bit) != 0) != rule.wired_when_set)
        .fold(Wiring::ALL, |wiring, rule| {
            wiring.without(rule.channel, rule.pin)
        })
}

const NCT_HWM_LDN: u8 = 0x0B;
const NCT_LDN_12: u8 = 0x12;
const NCT_LINUX_ID_MASK: u16 = 0xFFF8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NctMux {
    Unchecked,
    Nct6776,
    Nct6779,
    Nct6791,
    Nct6792,
    Nct6793,
    Nct6795,
    Nct6796,
    Nct6797,
    Nct6798,
    Nct6799,
}

impl NctMux {
    fn linux_id(self) -> Option<u16> {
        Some(match self {
            Self::Unchecked => return None,
            Self::Nct6776 => 0xC330,
            Self::Nct6779 => 0xC560,
            Self::Nct6791 => 0xC800,
            Self::Nct6792 => 0xC910,
            Self::Nct6793 => 0xD120,
            Self::Nct6795 => 0xD350,
            Self::Nct6796 => 0xD420,
            Self::Nct6797 => 0xD450,
            Self::Nct6798 => 0xD428,
            Self::Nct6799 => 0xD800,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct NctRegisters {
    cr1a: u8,
    cr1b: u8,
    cr1c: u8,
    cr1d: u8,
    cr24: u8,
    cr27: u8,
    cr2a: u8,
    cr2b: u8,
    cr2d: u8,
    cr2f: u8,
    hwm_enable: u8,
    hwm_e6: u8,
    ld12_e0: u8,
    ld12_eb: u8,
    ld12_ed: u8,
}

pub(crate) fn nct_wiring<P: PortIo>(
    space: &mut ConfigSpace<'_, P>,
    mux: NctMux,
    chip_id: u16,
    channels: usize,
) -> Result<Wiring, HwError> {
    if mux.linux_id() != Some(chip_id & NCT_LINUX_ID_MASK) {
        return Ok(Wiring::ALL);
    }
    let mut registers = NctRegisters {
        cr1a: space.read(0x1A)?,
        cr1b: space.read(0x1B)?,
        cr1c: space.read(0x1C)?,
        cr1d: space.read(0x1D)?,
        cr24: space.read(0x24)?,
        cr27: space.read(0x27)?,
        cr2a: space.read(0x2A)?,
        cr2b: space.read(0x2B)?,
        cr2d: space.read(0x2D)?,
        cr2f: space.read(0x2F)?,
        ..NctRegisters::default()
    };
    space.select(NCT_HWM_LDN)?;
    registers.hwm_enable = space.read(0x30)?;
    registers.hwm_e6 = space.read(0xE6)?;
    if mux != NctMux::Nct6776 {
        space.select(NCT_LDN_12)?;
        registers.ld12_e0 = space.read(0xE0)?;
        registers.ld12_eb = space.read(0xEB)?;
        registers.ld12_ed = space.read(0xED)?;
        space.select(NCT_HWM_LDN)?;
    }
    Ok(nct_pins(mux, &registers).wiring(channels))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct NctPins {
    fans: [bool; 7],
    pwms: [bool; 7],
}

impl NctPins {
    fn wiring(&self, channels: usize) -> Wiring {
        let mask = |pins: &[bool; 7]| {
            pins.iter()
                .take(channels)
                .enumerate()
                .fold(0u8, |mask, (channel, &wired)| {
                    mask | (u8::from(wired) << channel)
                })
        };
        Wiring {
            tachs: mask(&self.fans),
            pwms: mask(&self.pwms),
        }
    }
}

fn bit(value: u8, bit: u8) -> bool {
    value & (1 << bit) != 0
}

fn nct_pins(mux: NctMux, r: &NctRegisters) -> NctPins {
    let mut pins = NctPins {
        fans: [true, true, false, false, false, false, false],
        pwms: [true, true, false, false, false, false, false],
    };
    if mux == NctMux::Nct6776 {
        let fan3 = if bit(r.hwm_enable, 7) {
            bit(r.cr27, 7)
        } else {
            !bit(r.cr24, 6)
        };
        pins.fans[2] = fan3;
        pins.pwms[2] = fan3;
        return pins;
    }

    let dsw = bit(r.cr2f, 3);
    let ddr4 = bit(r.cr2f, 4);
    let as_seq1 = bit(r.cr2f, 7);
    let vsb_ctl = bit(r.cr2f, 0);
    let pwm7_pins_free = r.cr1d & 0x0C == 0;
    let eb_pwm = bit(r.ld12_eb, 4) && !bit(r.cr2a, 0);
    let e0_fan = !bit(r.cr1b, 0) && bit(r.ld12_e0, 3);
    let e0_pwm = !bit(r.cr1b, 0) && bit(r.ld12_e0, 4);
    let fan6_6795 = bit(r.cr2a, 4) && (!dsw || bit(r.ld12_ed, 4));
    let pwm6_6795 = bit(r.cr2a, 3) && bit(r.ld12_ed, 2);

    for channel in 2..5 {
        pins.fans[channel] = !bit(r.cr1c, channel as u8 + 3);
        pins.pwms[channel] = !bit(r.cr1c, channel as u8 - 2);
    }
    let [_, _, _, fan4, fan5, fan6, fan7] = &mut pins.fans;
    let [_, _, _, _, pwm5, pwm6, pwm7] = &mut pins.pwms;
    match mux {
        NctMux::Nct6791 => {
            *fan6 = bit(r.cr2d, 1);
            *pwm6 = bit(r.cr2d, 0);
        }
        NctMux::Nct6792 => {
            *fan6 = !dsw && bit(r.cr2d, 1);
            *pwm6 = !dsw && bit(r.cr2d, 0);
        }
        NctMux::Nct6793 => {
            *fan5 |= bit(r.cr1b, 5) || bit(r.ld12_eb, 5);
            *fan6 = (!dsw && bit(r.cr2d, 1)) || bit(r.ld12_eb, 3);
            *pwm5 |= bit(r.cr2d, 7) || eb_pwm;
            *pwm6 = (!dsw && bit(r.cr2d, 0)) || bit(r.ld12_eb, 2);
        }
        NctMux::Nct6795 => {
            *fan5 |= bit(r.cr1b, 5) || bit(r.ld12_eb, 5);
            *fan6 = fan6_6795 || bit(r.ld12_eb, 3);
            *pwm5 |= bit(r.cr2d, 7) || eb_pwm;
            *pwm6 = pwm6_6795 || bit(r.ld12_eb, 2);
        }
        NctMux::Nct6796 => {
            *fan5 |= bit(r.cr1b, 5) || e0_fan || bit(r.ld12_eb, 5);
            *fan6 = fan6_6795 || bit(r.ld12_eb, 3);
            *fan7 = !bit(r.cr2b, 2);
            *pwm5 |= bit(r.cr2d, 7) || e0_pwm || eb_pwm;
            *pwm6 = pwm6_6795 || bit(r.ld12_eb, 2);
            *pwm7 = pwm7_pins_free;
        }
        NctMux::Nct6797 => {
            *fan5 |= (!ddr4 && bit(r.cr1b, 5)) || bit(r.ld12_eb, 5);
            *fan6 = bit(r.cr2a, 4) || bit(r.ld12_eb, 3);
            *fan7 = bit(r.cr1a, 1);
            *pwm5 |= eb_pwm || (!ddr4 && bit(r.cr2d, 7));
            *pwm6 = bit(r.ld12_eb, 2) || bit(r.ld12_ed, 2);
            *pwm7 = bit(r.cr1d, 4);
        }
        NctMux::Nct6798 => {
            *fan6 = e0_fan || bit(r.cr2a, 4) || bit(r.ld12_eb, 5);
            *fan7 = bit(r.cr1b, 5) || !bit(r.cr2b, 2) || bit(r.ld12_eb, 3);
            *pwm6 = e0_pwm || (!bit(r.ld12_ed, 2) && bit(r.cr2a, 3)) || eb_pwm;
            *pwm7 = pwm7_pins_free || bit(r.cr2d, 7) || bit(r.ld12_eb, 2);
        }
        NctMux::Nct6799 => {
            *fan4 = bit(r.cr1c, 6);
            *fan5 = bit(r.cr1c, 7);
            *fan6 = e0_fan || bit(r.hwm_e6, 5) || bit(r.ld12_eb, 5) || (!as_seq1 && bit(r.cr2a, 4));
            *fan7 = bit(r.cr1b, 5) || (!vsb_ctl && !bit(r.cr2b, 2)) || bit(r.ld12_eb, 3);
            *pwm6 = e0_pwm
                || (!as_seq1 && !bit(r.ld12_ed, 2) && bit(r.cr2a, 3))
                || eb_pwm
                || bit(r.hwm_e6, 3);
            *pwm7 = (!vsb_ctl && pwm7_pins_free) || bit(r.ld12_eb, 2) || bit(r.cr2d, 7);
        }
        NctMux::Unchecked | NctMux::Nct6776 | NctMux::Nct6779 => {}
    }
    pins
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wired(wiring: Wiring, channels: usize) -> Vec<(bool, bool)> {
        (0..channels)
            .map(|channel| (wiring.has_tach(channel), wiring.has_pwm(channel)))
            .collect()
    }

    #[test]
    fn wiring_masks_answer_per_channel() {
        let wiring = Wiring::ALL
            .without(1, Pin::Pwm)
            .without(2, Pin::Tach)
            .without(3, Pin::Both);
        assert_eq!(
            wired(wiring, 5),
            [
                (true, true),
                (true, false),
                (false, true),
                (false, false),
                (true, true)
            ]
        );
        assert!(wiring.is_wired(1) && wiring.is_wired(2) && !wiring.is_wired(3));
        assert!(!Wiring::ALL.has_tach(8));
    }

    #[test]
    fn every_ite_rule_drops_only_its_own_pin() {
        for mux in [
            IteMux::It8620,
            IteMux::It8625,
            IteMux::It8655,
            IteMux::It8665,
            IteMux::Classic,
        ] {
            let rules = mux.rules();
            let mut wired = [0u8; 256];
            for rule in rules.iter().filter(|rule| rule.wired_when_set) {
                wired[usize::from(rule.register)] |= 1 << rule.bit;
            }
            assert_eq!(
                apply_rules(rules, 6, |register| wired[usize::from(register)]),
                Wiring::ALL,
                "{mux:?}"
            );
            for rule in rules {
                let mut flipped = wired;
                flipped[usize::from(rule.register)] ^= 1 << rule.bit;
                let read = |register: u8| flipped[usize::from(register)];
                assert_eq!(
                    apply_rules(rules, 6, read),
                    Wiring::ALL.without(rule.channel, rule.pin),
                    "{mux:?} {rule:?}"
                );
                assert_eq!(
                    apply_rules(rules, rule.channel, read),
                    Wiring::ALL,
                    "{mux:?} {rule:?}"
                );
            }
        }
    }

    #[test]
    fn nct6779_style_pins_follow_register_1c() {
        let registers = NctRegisters {
            cr1c: 0b1010_0010,
            ..NctRegisters::default()
        };
        assert_eq!(
            wired(nct_pins(NctMux::Nct6779, &registers).wiring(5), 5),
            [
                (true, true),
                (true, true),
                (false, true),
                (true, false),
                (false, true)
            ]
        );
    }

    #[test]
    fn nct6799_inverts_the_fan4_and_fan5_bits() {
        let registers = NctRegisters {
            cr1c: 0b0100_0000,
            cr2a: 0x10,
            ..NctRegisters::default()
        };
        let wiring = nct_pins(NctMux::Nct6799, &registers).wiring(7);
        assert!(wiring.has_tach(3));
        assert!(!wiring.has_tach(4));
        assert!(wiring.has_tach(5));
        assert!(wiring.has_pwm(6));
        let vsb = NctRegisters {
            cr2f: 0x01,
            ..registers
        };
        assert!(!nct_pins(NctMux::Nct6799, &vsb).wiring(7).has_pwm(6));
    }

    #[test]
    fn nct6798_finds_fan6_and_pwm6_on_any_of_their_pins() {
        let none = NctRegisters {
            cr2b: 0x04,
            cr1d: 0x0C,
            ld12_ed: 0x04,
            ..NctRegisters::default()
        };
        let wiring = nct_pins(NctMux::Nct6798, &none).wiring(7);
        assert!(!wiring.is_wired(5) && !wiring.is_wired(6));
        for registers in [
            NctRegisters {
                ld12_e0: 0x18,
                ..none
            },
            NctRegisters {
                ld12_eb: 0x30,
                cr2a: 0x10,
                ..none
            },
        ] {
            let wiring = nct_pins(NctMux::Nct6798, &registers).wiring(7);
            assert!(wiring.has_tach(5) && wiring.has_pwm(5));
        }
    }

    #[test]
    fn the_dsw_bit_takes_header_6_from_the_nct6792() {
        let registers = NctRegisters {
            cr2d: 0x03,
            ..NctRegisters::default()
        };
        assert!(nct_pins(NctMux::Nct6792, &registers).wiring(6).is_wired(5));
        let dsw = NctRegisters {
            cr2f: 0x08,
            ..registers
        };
        assert!(!nct_pins(NctMux::Nct6792, &dsw).wiring(6).is_wired(5));
        assert!(nct_pins(NctMux::Nct6791, &dsw).wiring(6).is_wired(5));
    }

    #[test]
    fn the_nct6776_takes_header_3_from_gp_or_register_24() {
        let free = NctRegisters::default();
        assert!(nct_pins(NctMux::Nct6776, &free).wiring(3).is_wired(2));
        let muxed = NctRegisters { cr24: 0x40, ..free };
        assert!(!nct_pins(NctMux::Nct6776, &muxed).wiring(3).is_wired(2));
        let gp = NctRegisters {
            hwm_enable: 0x80,
            cr27: 0x80,
            ..muxed
        };
        assert!(nct_pins(NctMux::Nct6776, &gp).wiring(3).is_wired(2));
    }

    #[test]
    fn headers_1_and_2_are_always_wired_on_nuvoton() {
        let all_set = NctRegisters {
            cr1a: 0xFF,
            cr1b: 0xFF,
            cr1c: 0xFF,
            cr1d: 0xFF,
            cr24: 0xFF,
            cr27: 0xFF,
            cr2a: 0xFF,
            cr2b: 0xFF,
            cr2d: 0xFF,
            cr2f: 0xFF,
            hwm_enable: 0xFF,
            hwm_e6: 0xFF,
            ld12_e0: 0xFF,
            ld12_eb: 0xFF,
            ld12_ed: 0xFF,
        };
        for mux in [
            NctMux::Nct6776,
            NctMux::Nct6779,
            NctMux::Nct6791,
            NctMux::Nct6792,
            NctMux::Nct6793,
            NctMux::Nct6795,
            NctMux::Nct6796,
            NctMux::Nct6797,
            NctMux::Nct6798,
            NctMux::Nct6799,
        ] {
            for registers in [all_set, NctRegisters::default()] {
                let wiring = nct_pins(mux, &registers).wiring(7);
                assert!(wiring.has_tach(0) && wiring.has_pwm(0), "{mux:?}");
                assert!(wiring.has_tach(1) && wiring.has_pwm(1), "{mux:?}");
            }
        }
    }

    #[test]
    fn only_rows_whose_id_linux_agrees_on_are_checked() {
        assert_eq!(NctMux::Nct6798.linux_id(), Some(0xD42B & NCT_LINUX_ID_MASK));
        assert_ne!(NctMux::Nct6796.linux_id(), Some(0xD42A & NCT_LINUX_ID_MASK));
        assert_eq!(NctMux::Nct6792.linux_id(), Some(0xC913 & NCT_LINUX_ID_MASK));
        assert_ne!(NctMux::Nct6779.linux_id(), Some(0xC56A & NCT_LINUX_ID_MASK));
        assert_eq!(NctMux::Unchecked.linux_id(), None);
    }
}
