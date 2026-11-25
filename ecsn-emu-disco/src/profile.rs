//! A sampling profile of the firmware: the function the core is in at each
//! sample, and the function its link register returns to.

use std::collections::HashMap;

use goblin::elf::{Elf, sym::STT_FUNC};

pub struct Profile {
    /// Start, end and name of each function, by start.
    functions: Vec<(u32, u32, String)>,
    /// Since the start, and since the last report of the interval.
    whole: Counts,
    interval: Counts,
}

/// Samples by function, and by function and caller.
#[derive(Default)]
struct Counts {
    samples: HashMap<usize, u64>,
    callers: HashMap<(usize, Option<usize>), u64>,
    total: u64,
}

impl Counts {
    fn add(&mut self, function: Option<usize>, caller: Option<usize>) {
        self.total += 1;
        if let Some(f) = function {
            *self.samples.entry(f).or_default() += 1;
            *self.callers.entry((f, caller)).or_default() += 1;
        }
    }
}

impl Profile {
    pub fn new(elf: &[u8]) -> Result<Self, String> {
        let elf = Elf::parse(elf).map_err(|e| format!("ELF: {e}"))?;
        let mut functions: Vec<(u32, u32, String)> = elf
            .syms
            .iter()
            .filter(|s| s.st_type() == STT_FUNC && s.st_value != 0)
            .filter_map(|s| {
                let start = (s.st_value as u32) & !1;
                let name = elf.strtab.get_at(s.st_name)?;
                Some((
                    start,
                    start + (s.st_size as u32).max(2),
                    format!("{:#}", rustc_demangle::demangle(name)),
                ))
            })
            .collect();
        functions.sort_by_key(|f| f.0);
        functions.dedup_by_key(|f| f.0);
        Ok(Self {
            functions,
            whole: Counts::default(),
            interval: Counts::default(),
        })
    }

    fn function(&self, address: u32) -> Option<usize> {
        let address = address & !1;
        let i = self.functions.partition_point(|f| f.0 <= address);
        let i = i.checked_sub(1)?;
        (address < self.functions[i].1).then_some(i)
    }

    /// One sample: the core at `pc`, its link register `lr`.
    pub fn sample(&mut self, pc: u32, lr: u32) {
        let f = self.function(pc);
        // An exception return value in LR is no caller.
        let caller = (lr < 0xF000_0000).then(|| self.function(lr)).flatten();
        self.whole.add(f, caller);
        self.interval.add(f, caller);
    }

    /// The `n` functions with the most samples since the last report of the
    /// interval, which starts a new one.
    pub fn interval_report(&mut self, n: usize) -> String {
        let counts = std::mem::take(&mut self.interval);
        self.report(&counts, n)
    }

    /// The `n` functions with the most samples since the start.
    pub fn whole_report(&self, n: usize) -> String {
        self.report(&self.whole, n)
    }

    /// The `n` functions with the most samples, each with its main callers.
    fn report(&self, counts: &Counts, n: usize) -> String {
        let mut out = format!("profile: {} samples\n", counts.total);
        let mut top: Vec<(usize, u64)> = counts.samples.iter().map(|(&f, &c)| (f, c)).collect();
        top.sort_by_key(|t| std::cmp::Reverse(t.1));
        let pct = |c: u64| 100.0 * c as f64 / counts.total.max(1) as f64;
        for (f, count) in top.into_iter().take(n) {
            out += &format!("{:5.1} %  {}\n", pct(count), self.functions[f].2);
            let mut callers: Vec<(Option<usize>, u64)> = counts
                .callers
                .iter()
                .filter(|((g, _), _)| *g == f)
                .map(|((_, c), &n)| (*c, n))
                .collect();
            callers.sort_by_key(|c| std::cmp::Reverse(c.1));
            for (caller, count) in callers.into_iter().take(3) {
                let name = caller.map_or("?", |c| self.functions[c].2.as_str());
                out += &format!("          {:5.1} % from {name}\n", pct(count));
            }
        }
        out
    }
}
