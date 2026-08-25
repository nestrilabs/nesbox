//! Helper functions for constructing GDT entries and converting to KVM segments

use kvm_bindings::kvm_segment;

/// Construct a conventional GDT entry from flags, base, and limit
pub fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 {
    ((base as u64 & 0xff00_0000) << (56 - 24))
        | ((flags as u64 & 0x0000_f0ff) << 40)
        | ((limit as u64 & 0x000f_0000) << (48 - 16))
        | ((base as u64 & 0x00ff_ffff) << 16)
        | (limit as u64 & 0x0000_ffff)
}

fn get_base(entry: u64) -> u64 {
    (((entry) & 0xFF00_0000_0000_0000) >> 32)
        | (((entry) & 0x0000_00FF_0000_0000) >> 16)
        | (((entry) & 0x0000_0000_FFFF_0000) >> 16)
}

fn get_limit(entry: u64) -> u32 {
    let limit: u32 =
        ((((entry) & 0x000F_0000_0000_0000) >> 32) | ((entry) & 0x0000_0000_0000_FFFF)) as u32;
    let g = ((entry & 0x0080_0000_0000_0000) >> 55) as u8;
    if g == 0 { limit } else { (limit << 12) | 0xFFF }
}

fn get_g(entry: u64) -> u8 {
    ((entry & 0x0080_0000_0000_0000) >> 55) as u8
}
fn get_db(entry: u64) -> u8 {
    ((entry & 0x0040_0000_0000_0000) >> 54) as u8
}
fn get_l(entry: u64) -> u8 {
    ((entry & 0x0020_0000_0000_0000) >> 53) as u8
}
fn get_avl(entry: u64) -> u8 {
    ((entry & 0x0010_0000_0000_0000) >> 52) as u8
}
fn get_p(entry: u64) -> u8 {
    ((entry & 0x0000_8000_0000_0000) >> 47) as u8
}
fn get_dpl(entry: u64) -> u8 {
    ((entry & 0x0000_6000_0000_0000) >> 45) as u8
}
fn get_s(entry: u64) -> u8 {
    ((entry & 0x0000_1000_0000_0000) >> 44) as u8
}
fn get_type(entry: u64) -> u8 {
    ((entry & 0x0000_0F00_0000_0000) >> 40) as u8
}

/// Convert a GDT entry (table index) to a KVM segment structure
pub fn kvm_segment_from_gdt(entry: u64, table_index: u8) -> kvm_segment {
    kvm_segment {
        base: get_base(entry),
        limit: get_limit(entry),
        selector: (table_index * 8).into(),
        type_: get_type(entry),
        present: get_p(entry),
        dpl: get_dpl(entry),
        db: get_db(entry),
        s: get_s(entry),
        l: get_l(entry),
        g: get_g(entry),
        avl: get_avl(entry),
        padding: 0,
        unusable: if get_p(entry) == 0 { 1 } else { 0 },
    }
}
