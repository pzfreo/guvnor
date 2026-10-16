//! Fan speeds from the SMC through IOKit, as reference/fan.c reads them. Needs no root.

use std::ffi::{c_char, c_void};

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Vers {
    major: u8,
    minor: u8,
    build: u8,
    reserved: u8,
    release: u16,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct PLimit {
    version: u16,
    length: u16,
    cpu: u32,
    gpu: u32,
    mem: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct KeyInfo {
    data_size: u32,
    data_type: u32,
    data_attributes: u8,
}

/// The classic AppleSMC user-client parameter block (IOConnectCallStructMethod selector 2).
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct SmcParam {
    key: u32,
    vers: Vers,
    plimit: PLimit,
    key_info: KeyInfo,
    result: u8,
    status: u8,
    data8: u8,
    data32: u32,
    bytes: [u8; 32],
}

const READ_BYTES: u8 = 5;
const READ_INDEX: u8 = 8;
const READ_KEY_INFO: u8 = 9;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceGetMatchingService(main_port: u32, matching: *const c_void) -> u32;
    fn IOServiceMatching(name: *const c_char) -> *const c_void;
    fn IOServiceOpen(service: u32, owning_task: u32, kind: u32, connect: *mut u32) -> i32;
    fn IOServiceClose(connect: u32) -> i32;
    fn IOObjectRelease(object: u32) -> i32;
    fn IOConnectCallStructMethod(
        connect: u32,
        selector: u32,
        input: *const c_void,
        input_size: usize,
        output: *mut c_void,
        output_size: *mut usize,
    ) -> i32;
    static mach_task_self_: u32;
}

fn four_cc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

pub struct Smc {
    conn: u32,
}

impl Smc {
    pub fn open() -> Option<Smc> {
        unsafe {
            let service = IOServiceGetMatchingService(0, IOServiceMatching(c"AppleSMC".as_ptr()));
            if service == 0 {
                return None;
            }
            let mut conn = 0;
            let rc = IOServiceOpen(service, mach_task_self_, 0, &mut conn);
            IOObjectRelease(service);
            (rc == 0).then_some(Smc { conn })
        }
    }

    fn call(&self, input: &SmcParam) -> Option<SmcParam> {
        let mut output = SmcParam::default();
        let mut size = size_of::<SmcParam>();
        let rc = unsafe {
            IOConnectCallStructMethod(
                self.conn,
                2,
                (input as *const SmcParam).cast(),
                size_of::<SmcParam>(),
                (&mut output as *mut SmcParam).cast(),
                &mut size,
            )
        };
        (rc == 0 && output.result == 0).then_some(output)
    }

    fn read(&self, key: &[u8; 4]) -> Option<SmcParam> {
        let mut input = SmcParam {
            key: four_cc(key),
            data8: READ_KEY_INFO,
            ..Default::default()
        };
        input.key_info = self.call(&input)?.key_info;
        input.data8 = READ_BYTES;
        let mut out = self.call(&input)?;
        out.key_info = input.key_info;
        Some(out)
    }

    /// A key's value: `flt ` on Apple silicon, `fpe2` on older Macs, or a single byte.
    pub fn number(&self, key: &[u8; 4]) -> Option<f64> {
        let p = self.read(key)?;
        let b = p.bytes;
        match p.key_info.data_type {
            t if t == four_cc(b"flt ") => Some(f32::from_ne_bytes([b[0], b[1], b[2], b[3]]) as f64),
            t if t == four_cc(b"fpe2") => Some(u16::from_be_bytes([b[0], b[1]]) as f64 / 4.0),
            _ if p.key_info.data_size == 1 => Some(b[0] as f64),
            _ => None,
        }
    }

    /// Every key the SMC has, by index (`#KEY` is the count).
    pub fn keys(&self) -> Vec<[u8; 4]> {
        let n = self
            .read(b"#KEY")
            .map(|p| u32::from_be_bytes([p.bytes[0], p.bytes[1], p.bytes[2], p.bytes[3]]))
            .unwrap_or(0);
        (0..n)
            .filter_map(|i| {
                let input = SmcParam {
                    data8: READ_INDEX,
                    data32: i,
                    ..Default::default()
                };
                self.call(&input).map(|p| p.key.to_be_bytes())
            })
            .collect()
    }

    /// The keys matching these names, a trailing `*` matching a prefix, that read as plausible
    /// temperatures now. Sensors stuck at one value are kept; there is no way to tell yet.
    pub fn resolve(&self, patterns: &[String]) -> Vec<[u8; 4]> {
        let all = self.keys();
        all.into_iter()
            .filter(|k| {
                patterns.iter().any(|p| match p.strip_suffix('*') {
                    Some(pre) => k.starts_with(pre.as_bytes()),
                    None => &k[..] == p.as_bytes(),
                })
            })
            .filter(|k| self.number(k).is_some_and(|v| v > 5.0 && v < 130.0))
            .collect()
    }

    /// The hottest of these sensors and their mean, in °C.
    pub fn temps(&self, keys: &[[u8; 4]]) -> Option<(f64, f64)> {
        let v: Vec<f64> = keys
            .iter()
            .filter_map(|k| self.number(k))
            .filter(|v| *v > 5.0 && *v < 130.0)
            .collect();
        let max = v.iter().copied().reduce(f64::max)?;
        Some((max, v.iter().sum::<f64>() / v.len() as f64))
    }

    /// Each fan's (actual, maximum) rpm.
    pub fn fans(&self) -> Vec<(f64, f64)> {
        let n = self.number(b"FNum").unwrap_or(0.0) as u8;
        (0..n.min(9))
            .map(|i| {
                let d = b'0' + i;
                let actual = self.number(&[b'F', d, b'A', b'c']).unwrap_or(-1.0);
                let max = self.number(&[b'F', d, b'M', b'x']).unwrap_or(-1.0);
                (actual, max)
            })
            .collect()
    }

    /// The fastest fan's rpm; 0 when all are off or unreadable.
    pub fn rpm(&self) -> u32 {
        self.fans()
            .iter()
            .map(|f| f.0.max(0.0) as u32)
            .max()
            .unwrap_or(0)
    }
}

impl Drop for Smc {
    fn drop(&mut self) {
        unsafe { IOServiceClose(self.conn) };
    }
}

/// `fan0 2312/7826 fan1 2495/7826`, as fan.c prints it.
pub fn describe(fans: &[(f64, f64)]) -> String {
    fans.iter()
        .enumerate()
        .map(|(i, (a, m))| format!("fan{i} {a:.0}/{m:.0}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn param_block_matches_the_c_layout() {
        assert_eq!(size_of::<SmcParam>(), 80);
        assert_eq!(std::mem::offset_of!(SmcParam, key_info), 28);
        assert_eq!(std::mem::offset_of!(SmcParam, data8), 42);
        assert_eq!(std::mem::offset_of!(SmcParam, bytes), 48);
    }
}
