//! Parameter name -> GRIB2 (discipline, category, number) table and level
//! type names -> Code Table 4.5 values. Source of codes: wgrib2 NCEP
//! gribtable.dat (see NOTES-fixtures.md).

#[derive(Clone, Debug)]
#[allow(clippy::redundant_static_lifetimes)]
pub struct ParamCode {
    name: &'static str,
    discipline: u8,
    category: u8,
    number: u8,
}

pub const PARAM_TABLE: [ParamCode; 15] = [
    ParamCode { name: "TMP", discipline: 0, category: 0, number: 0 },
    ParamCode { name: "HGT", discipline: 0, category: 3, number: 5 },
    ParamCode { name: "UGRD", discipline: 0, category: 2, number: 2 },
    ParamCode { name: "VGRD", discipline: 0, category: 2, number: 3 },
    ParamCode { name: "RH", discipline: 0, category: 1, number: 1 },
    ParamCode { name: "SPFH", discipline: 0, category: 1, number: 0 },
    ParamCode { name: "DPT", discipline: 0, category: 0, number: 6 },
    ParamCode { name: "PRMSL", discipline: 0, category: 3, number: 1 },
    ParamCode { name: "PRES", discipline: 0, category: 3, number: 0 },
    ParamCode { name: "VVEL", discipline: 0, category: 2, number: 8 },
    ParamCode { name: "APCP", discipline: 0, category: 1, number: 8 },
    ParamCode { name: "GUST", discipline: 0, category: 2, number: 22 },
    ParamCode { name: "CAPE", discipline: 7, category: 6, number: 0 },
    ParamCode { name: "CIN", discipline: 7, category: 7, number: 0 },
    ParamCode { name: "MSLMA", discipline: 0, category: 3, number: 198 },
];

/// Level type names -> Code Table 4.5 fixed-surface identifiers.
#[allow(clippy::redundant_static_lifetimes)]
pub const LEVEL_TYPES: [(&'static str, u8); 4] = [
    ("isobaric", 100),
    ("height_above_ground", 103),
    ("surface", 1),
    ("above_mean_sea_level", 101),
];

pub fn lookup_param(name: &str) -> Option<&ParamCode> {
    return PARAM_TABLE.iter().find(|pc| pc.name == name);
}

pub fn lookup_level_type(name: &str) -> Option<u8> {
    for pair in LEVEL_TYPES.iter() {
        if pair.0 == name {
            return Some(pair.1);
        }
    }
    return None;
}

impl ParamCode {
    pub fn describe(self: &ParamCode) -> String {
        return format!("{} d={}/c={}/n={}", self.name, self.discipline, self.category, self.number);
    }
}
