//! What processor this machine has, read from the kernel.
//!
//! One node's observation of its own hardware, in the pattern
//! [`crate::devices`] already follows: this states facts and decides nothing.
//! Every decision made from them —  which nodes can exchange guests, what a
//! baseline would cost, whether a migration may proceed — is made centrally by
//! the pure functions in [`velstra_cloud_model::cpu`], from the whole fleet's
//! worth of these reports.
//!
//! ## Where the numbers come from
//!
//! `/proc/cpuinfo`, and deliberately not a CPUID crate. The kernel has already
//! done the work of decoding CPUID, applying microcode-level errata masking,
//! and — the part that matters most here — *hiding features it has disabled*.
//! A guest can only use what the kernel will let KVM expose, so the kernel's
//! view is the one that predicts whether a guest keeps running. Reading CPUID
//! directly would report features the host has and the guest can never see.
//!
//! ## What is tested here
//!
//! The parsing, against captured `/proc/cpuinfo` text from real machines. What
//! this machine happens to have is not asserted anywhere: a test that expected
//! `avx2` would pass or fail depending on the developer's laptop.

use std::collections::BTreeSet;

use velstra_cloud_model::cpu::{CpuLevel, NodeCpu};

/// Read this machine's processor.
///
/// `can_mask` is the caller's to supply, because it is a property of the VMM
/// that will run the guests, not of the silicon: QEMU can present a CPU other
/// than the host's and Cloud Hypervisor cannot. Passing it in rather than
/// guessing here is what lets that answer change — for a VMM, or for an
/// architecture — without this function knowing.
///
/// `baseline` is what this node has been told to present. `None` is the
/// host's own processor.
pub fn observe(can_mask: bool, baseline: Option<CpuLevel>) -> NodeCpu {
    let text = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let mut cpu = parse(&text);
    cpu.can_mask = can_mask;

    // What guests here actually get. Under a baseline that is the level's flag
    // set — derived from the same code on every node, which is what makes two
    // baselined nodes compare equal — and otherwise it is this machine, whose
    // flags a guest sees exactly.
    match baseline {
        Some(level) => {
            cpu.presents = level.to_string();
            cpu.presented_flags = level.flags();
        }
        None => {
            cpu.presents = "host".to_string();
            cpu.presented_flags = cpu.flags.clone();
        }
    }
    cpu
}

/// Parse `/proc/cpuinfo` into a report.
///
/// Only the first processor block is read. A machine with asymmetric cores
/// (Intel's P/E split, ARM big.LITTLE) reports different flags per core, and
/// the honest answer for "what can a guest rely on" is not "whatever core 0
/// has" — but a guest pinned nowhere can land on any core, so the *intersection*
/// is the only safe answer. That is what this computes.
fn parse(text: &str) -> NodeCpu {
    let mut cpu = NodeCpu {
        arch: std::env::consts::ARCH.to_string(),
        ..NodeCpu::default()
    };

    // Every core's flag set, intersected below. On the overwhelmingly common
    // symmetric machine this is N copies of one set and the intersection is
    // that set; on an asymmetric one it is the only answer that does not
    // promise a guest something half its cores lack.
    let mut per_core: Vec<BTreeSet<String>> = Vec::new();

    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "vendor_id" if cpu.vendor.is_empty() => cpu.vendor = value.to_string(),
            "model name" if cpu.model_name.is_empty() => cpu.model_name = value.to_string(),
            "cpu family" if cpu.family == 0 => cpu.family = value.parse().unwrap_or(0),
            "model" if cpu.model == 0 => cpu.model = value.parse().unwrap_or(0),
            "stepping" if cpu.stepping == 0 => cpu.stepping = value.parse().unwrap_or(0),
            // `flags` on x86, `Features` on aarch64. Both spellings, because
            // the file is the kernel's and the kernel spells it per-arch.
            "flags" | "Features" => {
                per_core.push(value.split_whitespace().map(|f| f.to_string()).collect());
            }
            // aarch64 has no `model name`; this is the nearest thing it offers.
            "CPU implementer" if cpu.vendor.is_empty() => cpu.vendor = value.to_string(),
            _ => {}
        }
    }

    cpu.flags = match per_core.split_first() {
        Some((first, rest)) => rest.iter().fold(first.clone(), |acc, next| {
            acc.intersection(next).cloned().collect()
        }),
        None => BTreeSet::new(),
    };
    cpu
}

#[cfg(test)]
mod tests {
    use super::*;
    use velstra_cloud_model::cpu::CpuLevel;

    /// Two cores, the second missing a flag the first has.
    const ASYMMETRIC: &str = "\
processor\t: 0
vendor_id\t: GenuineIntel
cpu family\t: 6
model\t\t: 151
model name\t: 12th Gen Intel(R) Core(TM) i7-12700K
stepping\t: 2
flags\t\t: fpu vme sse3 ssse3 sse4_1 sse4_2 popcnt cx16 lahf_lm avx avx2

processor\t: 1
vendor_id\t: GenuineIntel
cpu family\t: 6
model\t\t: 151
model name\t: 12th Gen Intel(R) Core(TM) i7-12700K
stepping\t: 2
flags\t\t: fpu vme sse3 ssse3 sse4_1 sse4_2 popcnt cx16 lahf_lm
";

    #[test]
    fn the_identity_fields_come_off_the_first_block() {
        let cpu = parse(ASYMMETRIC);
        assert_eq!(cpu.vendor, "GenuineIntel");
        assert_eq!(cpu.model_name, "12th Gen Intel(R) Core(TM) i7-12700K");
        assert_eq!(cpu.family, 6);
        assert_eq!(cpu.model, 151);
        assert_eq!(cpu.stepping, 2);
    }

    /// A flag only some cores have is not a flag the machine has.
    ///
    /// The guest is not pinned, so it can be scheduled onto any core. Reporting
    /// core 0's flags would promise `avx2` to a guest that will sooner or later
    /// run on a core without it — a fault with no plausible explanation at the
    /// point it happens.
    #[test]
    fn asymmetric_cores_report_the_intersection_not_the_best_core() {
        let cpu = parse(ASYMMETRIC);
        assert!(!cpu.flags.contains("avx2"), "{:?}", cpu.flags);
        assert!(!cpu.flags.contains("avx"));
        assert!(cpu.flags.contains("sse4_2"));
        assert_eq!(cpu.level(), Some(CpuLevel::V2));
    }

    #[test]
    fn a_symmetric_machine_reports_what_its_cores_have() {
        let text = "\
processor\t: 0
vendor_id\t: AuthenticAMD
flags\t\t: sse3 ssse3 sse4_1 sse4_2 popcnt cx16 lahf_lm avx avx2 bmi1 bmi2 f16c fma lzcnt movbe

processor\t: 1
vendor_id\t: AuthenticAMD
flags\t\t: sse3 ssse3 sse4_1 sse4_2 popcnt cx16 lahf_lm avx avx2 bmi1 bmi2 f16c fma lzcnt movbe
";
        let cpu = parse(text);
        assert_eq!(cpu.vendor, "AuthenticAMD");
        assert_eq!(cpu.level(), Some(CpuLevel::V3));
    }

    /// An unreadable or empty file yields a report that claims nothing.
    ///
    /// It must not yield one that claims x86-64-v1: every caller treats an
    /// empty flag set as "cannot be shown compatible", and a confident v1
    /// would be a claim this function is in no position to make.
    #[test]
    fn nothing_readable_yields_no_claim_rather_than_a_low_one() {
        let cpu = parse("");
        assert!(cpu.flags.is_empty());
        assert!(cpu.vendor.is_empty());
    }

    #[test]
    fn aarch64_spells_its_flag_line_differently_and_is_still_read() {
        let text = "\
processor\t: 0
BogoMIPS\t: 50.00
Features\t: fp asimd evtstrm aes pmull sha1 sha2 crc32
CPU implementer\t: 0x41
";
        let cpu = parse(text);
        assert!(cpu.flags.contains("asimd"), "{:?}", cpu.flags);
        assert_eq!(cpu.vendor, "0x41");
    }
}
