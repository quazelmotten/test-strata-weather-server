//! On-disk storage format (spec §4.5): raw f32 little-endian, layout
//! `[level][y][x]`, small fixed header per .bin file. Reader side uses memmap2
//! at the server layer; this module only defines the format constants and
//! header codec (milestone 1 completes full read/write).

/// .bin header (fixed 24 bytes, all BE):
///   magic u32 = 0x57585031 ("WXPO"), version u16 = 1, flags u16,
///   nx u32, ny u32, nlevels u32, then nlevels * i4 level coords (micro-units).
pub const BIN_MAGIC: u32 = 0x5758_5031;
pub const BIN_VERSION: u16 = 1;

#[derive(Clone, Debug)]
pub struct BinHeader {
    nx: u32,
    ny: u32,
    levels: Vec<f64>, // sorted by pressure (spec §4.5)
}

impl BinHeader {
    pub fn describe(self: &BinHeader) -> String {
        return format!("bin {}x{} levels={}", self.nx, self.ny, self.levels.len());
    }
}
