//! Build a `/proc/kallsyms`-style text blob for the running macOS
//! kernel.
//!
//! Two parts:
//!
//! 1. [`KernelImage::load`] parses the on-disk kernel binary
//!    (`/System/Library/Kernels/kernel.release.t<chip>`), extracts
//!    its `LC_SYMTAB` (raw SVMAs) and the executable-segment ranges
//!    we'll use as constraints for slide derivation.
//! 2. [`SlideEstimator`] watches kernel addresses observed in kperf
//!    samples and votes for the 16KB-aligned KASLR slide that fits
//!    the most samples. We have to do this because the proper
//!    public API for getting the slide (`kas_info(KAS_INFO_KERNEL_TEXT_SLIDE_SELECTOR)`)
//!    is gated behind `com.apple.private.kernel.get-kernel-info`,
//!    an Apple-internal entitlement we can't sign with.
//!
//! Once a slide is settled, [`KernelImage::format_kallsyms`] emits
//! `/proc/kallsyms`-style text that nperf's `data_reader` picks up
//! via the existing `kallsyms::parse` path.

use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::PathBuf;

use object::macho::{MachHeader64, N_SECT, N_STAB, N_TYPE};
use object::read::macho::{LoadCommandVariant, MachHeader as _, Nlist as _};
use object::LittleEndian;

use nerf_mac_kperf_sys::error::Error;

/// KASLR slides on arm64-darwin are page-aligned at 16KB.
const SLIDE_ALIGNMENT: u64 = 0x4000;

#[derive(Debug)]
pub struct KernelImage {
    /// Path the binary was loaded from (for logging).
    pub path: PathBuf,
    /// `(svma, name)` pairs harvested from `LC_SYMTAB`. Filtered to
    /// `N_SECT` symbols with non-empty names; SVMAs are unslid.
    pub symbols: Vec<(u64, String)>,
    /// Half-open ranges of all executable segments by SVMA. Used by
    /// the slide estimator and bound-checked when emitting kallsyms.
    pub exec_segments: Vec<(u64, u64)>,
}

impl KernelImage {
    pub fn load() -> Result<Option<Self>, Error> {
        let kver = read_sysctl_string(b"kern.version\0")?;
        let chip = match extract_chip(&kver) {
            Some(c) => c,
            None => {
                log::warn!(
                    "could not extract chip from kern.version={kver:?}; \
                     skipping kernel symbol load"
                );
                return Ok(None);
            }
        };
        let path = PathBuf::from(format!(
            "/System/Library/Kernels/kernel.release.t{}",
            chip.to_lowercase()
        ));
        log::info!("loading kernel symbols from {}", path.display());
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(err) => {
                log::warn!("read({}) failed: {err}", path.display());
                return Ok(None);
            }
        };

        let header: &MachHeader64<LittleEndian> =
            match MachHeader64::parse(bytes.as_slice(), 0) {
                Ok(h) => h,
                Err(err) => {
                    log::warn!("MachHeader64::parse: {err}");
                    return Ok(None);
                }
            };
        let endian = match header.endian() {
            Ok(e) => e,
            Err(err) => {
                log::warn!("header.endian: {err}");
                return Ok(None);
            }
        };

        let mut exec_segments = Vec::new();
        let mut symtab_cmd = None;
        let mut load_commands =
            match header.load_commands(endian, bytes.as_slice(), 0) {
                Ok(lc) => lc,
                Err(err) => {
                    log::warn!("load_commands: {err}");
                    return Ok(None);
                }
            };
        while let Ok(Some(command)) = load_commands.next() {
            let variant = match command.variant() {
                Ok(v) => v,
                Err(_) => continue,
            };
            match variant {
                LoadCommandVariant::Segment64(seg, _) => {
                    let initprot = seg.initprot.get(endian);
                    // VM_PROT_EXECUTE = 4
                    if initprot & 4 != 0 {
                        let lo = seg.vmaddr.get(endian);
                        let hi = lo.wrapping_add(seg.vmsize.get(endian));
                        if hi > lo {
                            let segname: &[u8] = &seg.segname;
                            log::debug!(
                                "kernel exec segment {:?} {:#x}..{:#x}",
                                std::str::from_utf8(
                                    segname
                                        .split(|&b| b == 0)
                                        .next()
                                        .unwrap_or(segname)
                                )
                                .unwrap_or("?"),
                                lo,
                                hi
                            );
                            exec_segments.push((lo, hi));
                        }
                    }
                }
                LoadCommandVariant::Symtab(cmd) => {
                    symtab_cmd = Some(cmd);
                }
                _ => {}
            }
        }
        let symtab_cmd = match symtab_cmd {
            Some(s) => s,
            None => {
                log::warn!("no LC_SYMTAB in kernel binary");
                return Ok(None);
            }
        };
        let symbols_table = match symtab_cmd
            .symbols::<MachHeader64<LittleEndian>, _>(endian, bytes.as_slice())
        {
            Ok(s) => s,
            Err(err) => {
                log::warn!("symtab parse failed: {err}");
                return Ok(None);
            }
        };
        let strings = symbols_table.strings();
        let mut symbols = Vec::with_capacity(symbols_table.iter().len());
        for symbol in symbols_table.iter() {
            let n_type = symbol.n_type();
            if n_type & N_STAB != 0 {
                continue;
            }
            if n_type & N_TYPE != N_SECT {
                continue;
            }
            let n_value = symbol.n_value(endian);
            if n_value == 0 {
                continue;
            }
            let name = match symbol.name(endian, strings) {
                Ok(n) if !n.is_empty() => n,
                _ => continue,
            };
            let name_str = match std::str::from_utf8(name) {
                Ok(s) => s.to_string(),
                Err(_) => continue,
            };
            symbols.push((n_value, name_str));
        }
        log::info!(
            "kernel image: {} exec segments, {} text symbols",
            exec_segments.len(),
            symbols.len()
        );
        if exec_segments.is_empty() || symbols.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self {
            path,
            symbols,
            exec_segments,
        }))
    }

    /// Render `/proc/kallsyms` text with the given slide applied.
    pub fn format_kallsyms(&self, slide: u64) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.symbols.len() * 64);
        for (svma, name) in &self.symbols {
            let avma = svma.wrapping_add(slide);
            let _ = writeln!(&mut buf, "{avma:016x} T {name}");
        }
        buf
    }
}

/// Iteratively narrows down the kernel KASLR slide by voting:
/// every observed kernel sample address contributes votes for
/// every 16KB-aligned slide that would place it inside any of
/// the kernel's executable segments. The slide with the most
/// votes after enough samples is the actual slide.
pub struct SlideEstimator {
    exec_segments: Vec<(u64, u64)>,
    votes: HashMap<u64, u32>,
    observed: u32,
}

impl SlideEstimator {
    pub fn new(exec_segments: Vec<(u64, u64)>) -> Self {
        Self {
            exec_segments,
            votes: HashMap::new(),
            observed: 0,
        }
    }

    /// Observe one runtime kernel address. Cheap.
    pub fn observe(&mut self, avma: u64) {
        self.observed += 1;
        for &(seg_lo, seg_hi) in &self.exec_segments {
            // svma must be in [seg_lo, seg_hi); slide = avma - svma.
            // => slide must be in (avma - seg_hi, avma - seg_lo].
            if avma < seg_lo {
                continue;
            }
            let slide_max = avma - seg_lo;
            let slide_min = if avma >= seg_hi {
                avma - seg_hi + 1
            } else {
                0
            };
            // Snap to 16KB alignment.
            let lo = (slide_min + (SLIDE_ALIGNMENT - 1)) & !(SLIDE_ALIGNMENT - 1);
            let hi = slide_max & !(SLIDE_ALIGNMENT - 1);
            // Cap iteration to a sane number of candidates per segment so
            // a hostile or unexpected sample can't blow up the table.
            const MAX_PER_OBS: u64 = 4096;
            let n = (hi.saturating_sub(lo)) / SLIDE_ALIGNMENT;
            if n > MAX_PER_OBS {
                continue;
            }
            let mut s = lo;
            while s <= hi {
                *self.votes.entry(s).or_insert(0) += 1;
                s += SLIDE_ALIGNMENT;
            }
        }
    }

    pub fn observed_count(&self) -> u32 {
        self.observed
    }

    /// Pick the slide with the highest support. Returns the slide
    /// and the fraction of observed samples it explains.
    pub fn finalize(&self) -> Option<(u64, f64)> {
        let total = self.observed.max(1);
        self.votes
            .iter()
            .max_by_key(|(_, c)| **c)
            .map(|(slide, count)| (*slide, *count as f64 / total as f64))
    }
}

fn extract_chip(kver: &str) -> Option<String> {
    let needle = "RELEASE_ARM64_T";
    let start = kver.rfind(needle)? + needle.len();
    let chip: String = kver[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    if chip.is_empty() {
        None
    } else {
        Some(chip)
    }
}

fn read_sysctl_string(name_with_nul: &[u8]) -> Result<String, Error> {
    debug_assert!(name_with_nul.ends_with(b"\0"));
    let mut size: usize = 0;
    let rc = unsafe {
        libc::sysctlbyname(
            name_with_nul.as_ptr() as *const _,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc < 0 || size == 0 {
        return Err(Error::Sysctl {
            op: "sysctlbyname (size)",
            source: std::io::Error::last_os_error(),
        });
    }
    let mut buf = vec![0u8; size];
    let rc = unsafe {
        libc::sysctlbyname(
            name_with_nul.as_ptr() as *const _,
            buf.as_mut_ptr() as *mut _,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc < 0 {
        return Err(Error::Sysctl {
            op: "sysctlbyname (read)",
            source: std::io::Error::last_os_error(),
        });
    }
    while buf.last() == Some(&0) {
        buf.pop();
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}
