use std::mem::{self, ManuallyDrop};
use std::ops::{Index, Range};
use std::str;
use std::sync::Arc;
use std::time::Instant;

use crate::binary::{BinaryData, SymbolTable};
use crate::elf::{self, Endian, Strtab};
use crate::range_map::RangeMap;
use crate::types::{Bitness, Endianness};
use crate::utils::{get_ms, StableIndex};

trait ByteContainer: StableIndex + Index<Range<u64>, Output = [u8]> + 'static {}
impl<T> ByteContainer for T where T: StableIndex + Index<Range<u64>, Output = [u8]> + 'static {}

pub struct Symbols {
    strtab_owner: ManuallyDrop<Arc<dyn ByteContainer<Output = [u8]> + Send + Sync>>,
    symbols: ManuallyDrop<RangeMap<&'static str>>,
}

impl Drop for Symbols {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            ManuallyDrop::drop(&mut self.symbols);
            ManuallyDrop::drop(&mut self.strtab_owner);
        }
    }
}

fn load_symbols<'a, F: FnMut(Range<u64>, &'a str)>(
    architecture: &str,
    bitness: Bitness,
    endianness: Endianness,
    sym_bytes: &[u8],
    strtab_bytes: &'a [u8],
    mut callback: F,
) {
    let is_arm = architecture == "arm";
    let endian = match endianness {
        Endianness::LittleEndian => Endian::Little,
        Endianness::BigEndian => Endian::Big,
    };

    let strtab = Strtab::new(strtab_bytes, 0x0);
    let syms = elf::SymIter::new(sym_bytes, endian, bitness == Bitness::B64);
    for sym in syms {
        if !sym.is_function() || sym.st_size == 0 || sym.st_value == 0 {
            continue;
        }
        if let Some(Ok(name)) = strtab.get(sym.st_name) {
            let mut start = sym.st_value;
            if is_arm {
                // On ARM the lowest bit of the symbol value specifies
                // whether the instruction it points at is an ARM or a
                // Thumb one, so we mask it out.
                // Source: ELF for the ARM Architecture
                //         http://infocenter.arm.com/help/topic/com.arm.doc.ihi0044f/IHI0044F_aaelf.pdf
                start &= !1;
            }

            let end = start + sym.st_size;
            callback(start..end, name);
        }
    }
}

impl Symbols {
    pub fn load_from_binary_data(data: &Arc<BinaryData>) -> Self {
        Symbols::load(
            data.name(),
            data.architecture(),
            data.bitness(),
            data.endianness(),
            data.symbol_tables(),
            &**data,
            data,
        )
    }

    pub fn each_from_binary_data<F: FnMut(Range<u64>, &str)>(data: &BinaryData, mut callback: F) {
        for symbol_table in data.symbol_tables() {
            let sym_bytes = &data[symbol_table.range.clone()];
            let strtab_bytes = &data[symbol_table.strtab_range.clone()];

            load_symbols(
                data.architecture(),
                data.bitness(),
                data.endianness(),
                sym_bytes,
                strtab_bytes,
                |range, name| {
                    callback(range, name);
                },
            );
        }
    }

    pub fn load<T, S>(
        name: &str,
        architecture: &str,
        bitness: Bitness,
        endianness: Endianness,
        symbol_tables: &[SymbolTable],
        symbol_tables_bytes: &S,
        strtab_owner: &Arc<T>,
    ) -> Self
    where
        S: ?Sized + Index<Range<u64>, Output = [u8]>,
        T: StableIndex + Index<Range<u64>, Output = [u8]> + Send + Sync + 'static,
    {
        let start_timestamp = Instant::now();

        let mut symbols: Vec<(Range<u64>, &str)> = Vec::new();
        let mut normal_count = 0;
        let mut dynamic_count = 0;

        for symbol_table in symbol_tables {
            let sym_bytes = &symbol_tables_bytes[symbol_table.range.clone()];
            let strtab_bytes = &strtab_owner[symbol_table.strtab_range.clone()];

            let count_before = symbols.len();
            load_symbols(
                architecture,
                bitness,
                endianness,
                sym_bytes,
                strtab_bytes,
                |range, name| {
                    symbols.push((range, name));
                },
            );

            let count = symbols.len() - count_before;
            if symbol_table.is_dynamic {
                dynamic_count += count;
            } else {
                normal_count += count;
            }
        }

        let symbols: Vec<(Range<u64>, &'static str)> = unsafe { mem::transmute(symbols) };
        let elapsed = start_timestamp.elapsed();
        debug!(
            "Loaded {} symbols for '{}' ({} normal, {} dynamic) in {}ms",
            symbols.len(),
            name,
            normal_count,
            dynamic_count,
            get_ms(elapsed)
        );
        let symbols = Symbols {
            strtab_owner: ManuallyDrop::new(strtab_owner.clone()),
            symbols: ManuallyDrop::new(RangeMap::from_vec(symbols)),
        };

        debug_assert!(symbols.is_owned_by(strtab_owner));
        symbols
    }

    #[inline]
    fn as_range_map(&self) -> &RangeMap<&str> {
        &self.symbols
    }

    #[inline]
    pub fn get_symbol(&self, address: u64) -> Option<(Range<u64>, &str)> {
        self.as_range_map()
            .get(address)
            .map(|(range, name)| (range, *name))
    }

    #[inline]
    pub fn get_symbol_index(&self, address: u64) -> Option<usize> {
        self.as_range_map().get_index(address)
    }

    #[inline]
    pub fn get_symbol_by_index(&self, index: usize) -> Option<(Range<u64>, &str)> {
        self.as_range_map()
            .get_by_index(index)
            .map(|(range, name)| (range, *name))
    }

    #[inline]
    pub fn is_owned_by<T>(&self, strtab_owner: &Arc<T>) -> bool
    where
        T: StableIndex + Index<Range<u64>, Output = [u8]> + 'static,
    {
        let lhs: &dyn ByteContainer<Output = [u8]> = &**self.strtab_owner;
        let rhs: &dyn ByteContainer<Output = [u8]> = &**strtab_owner;
        to_ptr(lhs) == to_ptr(rhs)
    }

    /// Build a `Symbols` from already-resolved `(start..end, name)` entries.
    ///
    /// Useful when the parser is upstream of nwind (e.g. mac records that
    /// pre-resolve Mach-O `LC_SYMTAB` at capture time, since the unwind side
    /// of nwind doesn't know Mach-O). The names are concatenated into a
    /// single owned buffer so the final `&'static str` references are valid
    /// for the lifetime of the resulting `Symbols`.
    pub fn from_resolved_entries<I, S>(entries: I) -> Self
    where
        I: IntoIterator<Item = (Range<u64>, S)>,
        S: AsRef<str>,
    {
        let mut buffer: Vec<u8> = Vec::new();
        let mut staged: Vec<(Range<u64>, Range<usize>)> = Vec::new();
        for (range, name) in entries {
            let bytes = name.as_ref().as_bytes();
            let start = buffer.len();
            buffer.extend_from_slice(bytes);
            let end = buffer.len();
            staged.push((range, start..end));
        }

        let owner: Arc<OwnedBuffer> = Arc::new(OwnedBuffer { bytes: buffer });

        // SAFETY: the Arc<OwnedBuffer> we store as `strtab_owner` keeps the
        // backing bytes alive for as long as `Symbols` is alive. The
        // `&'static str` references below actually point into that Arc's
        // buffer, so the static lifetime is a stand-in for "as long as the
        // Symbols exists" -- the same trick `load()` uses above.
        let symbols: Vec<(Range<u64>, &'static str)> = staged
            .into_iter()
            .filter_map(|(range, str_range)| {
                let bytes = &owner.bytes[str_range];
                let s: &str = std::str::from_utf8(bytes).ok()?;
                let s: &'static str = unsafe { mem::transmute(s) };
                Some((range, s))
            })
            .collect();

        Symbols {
            strtab_owner: ManuallyDrop::new(owner),
            symbols: ManuallyDrop::new(RangeMap::from_vec(symbols)),
        }
    }
}

struct OwnedBuffer {
    bytes: Vec<u8>,
}

impl Index<Range<u64>> for OwnedBuffer {
    type Output = [u8];
    fn index(&self, range: Range<u64>) -> &[u8] {
        &self.bytes[range.start as usize..range.end as usize]
    }
}

unsafe impl StableIndex for OwnedBuffer {}

#[inline]
fn to_ptr<T: ?Sized>(reference: &T) -> *const u8 {
    reference as *const T as *const u8
}
